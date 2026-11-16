#!/usr/bin/env /usr/bin/python3
"""Print the path of the current track's cover (cached); print nothing when there is none.

Remote covers are fetched only from public addresses (DNS result validated, then that exact address
is connected), with a byte cap and a total deadline. Cache files live in the private runtime directory."""
import hashlib
import importlib.util
import http.client
import ipaddress
import os
import re
import secrets
import socket
import ssl
import stat
import subprocess
import sys
import time
import urllib.parse
from importlib.machinery import SourceFileLoader

PLAYERS = 'spotify,%any'
MAX_BYTES = 8 * 1024 * 1024
CHUNK = 64 * 1024
SOCKET_TIMEOUT = 3.0
TOTAL_DEADLINE = 10.0
MAX_REDIRECTS = 10
PREFIX = 'media-art-'
CACHE_RE = re.compile(r'^media-art-[0-9a-f]{64}$')

BLOCKED_V4 = [ipaddress.ip_network(n) for n in (
    '0.0.0.0/8', '10.0.0.0/8', '100.64.0.0/10', '127.0.0.0/8', '169.254.0.0/16', '172.16.0.0/12',
    '192.0.0.0/24', '192.0.2.0/24', '192.88.99.0/24', '192.168.0.0/16', '198.18.0.0/15',
    '198.51.100.0/24', '203.0.113.0/24', '224.0.0.0/4', '240.0.0.0/4')]
V6_GLOBAL = ipaddress.ip_network('2000::/3')
BLOCKED_V6 = [ipaddress.ip_network(n) for n in ('2001::/23', '2001:db8::/32', '2002::/16', '3fff::/20')]


class ArtError(Exception):
    """Fetch/cache failure; the message never contains URLs, bodies or private paths."""


class PolicyError(ArtError):
    """Destination refused by policy: no request is made and nothing is retried."""


def plugin():
    here = os.path.dirname(os.path.realpath(__file__))
    loader = SourceFileLoader('waybar_pill_plugin', os.path.join(here, 'waybar-pill-plugin'))
    mod = importlib.util.module_from_spec(importlib.util.spec_from_loader('waybar_pill_plugin', loader))
    loader.exec_module(mod)
    return mod


def address_allowed(text):
    try:
        ip = ipaddress.ip_address(text)
    except ValueError:
        return False
    if ip.version == 6 and ip.ipv4_mapped is not None:
        ip = ip.ipv4_mapped
    if ip.version == 4:
        return not any(ip in n for n in BLOCKED_V4)
    return ip in V6_GLOBAL and not any(ip in n for n in BLOCKED_V6)


def cache_name(url):
    return PREFIX + hashlib.sha256(url.encode()).hexdigest()


def resolve_public(host, port, resolver=socket.getaddrinfo):
    """Resolve once; refuse the whole answer when any address is disallowed. Returns [(family, sockaddr)]."""
    try:
        infos = resolver(host, port, type=socket.SOCK_STREAM)
    except OSError:
        raise ArtError('name resolution failed') from None
    out = []
    for family, _t, _p, _c, sockaddr in infos:
        if family not in (socket.AF_INET, socket.AF_INET6):
            continue
        if not address_allowed(sockaddr[0]):
            raise PolicyError('destination is not a public address')
        out.append((family, sockaddr))
    if not out:
        raise ArtError('name resolution returned no usable address')
    return out


def default_connect(family, sockaddr, timeout):
    s = socket.socket(family, socket.SOCK_STREAM)
    try:
        s.settimeout(timeout)
        s.connect(sockaddr)
    except OSError:
        s.close()
        raise
    return s


def _remaining(deadline, clock):
    left = deadline - clock()
    if left <= 0:
        raise ArtError('deadline exceeded')
    return left


def _check_url(url, previous=None):
    p = urllib.parse.urlsplit(url)
    if p.scheme not in ('http', 'https') or not p.hostname:
        raise PolicyError('unsupported URL')
    if p.username is not None or p.password is not None or '@' in p.netloc:
        raise PolicyError('URL carries credentials')
    if previous is not None and previous.scheme == 'https' and p.scheme == 'http':
        raise PolicyError('https to http downgrade')
    try:
        port = p.port or (443 if p.scheme == 'https' else 80)
    except ValueError:
        raise PolicyError('bad port') from None
    return p, port


def open_response(url, deadline, resolver=socket.getaddrinfo, connect=default_connect,
                  tls_context=None, clock=time.monotonic):
    """Follow up to ten redirects manually; every hop is validated before any connection.
    Returns (connection, response) for the final 2xx answer."""
    previous = None
    for _hop in range(MAX_REDIRECTS + 1):
        p, port = _check_url(url, previous)
        addrs = resolve_public(p.hostname, port, resolver)
        sock = None
        last = None
        for family, sockaddr in addrs:
            try:
                sock = connect(family, sockaddr, min(SOCKET_TIMEOUT, _remaining(deadline, clock)))
                break
            except ArtError:
                raise
            except OSError as e:
                last = e
        if sock is None:
            raise ArtError('connection failed') from None
        try:
            if p.scheme == 'https':
                ctx = tls_context or ssl.create_default_context()
                sock = ctx.wrap_socket(sock, server_hostname=p.hostname)
                conn = http.client.HTTPSConnection(p.hostname, port, timeout=SOCKET_TIMEOUT, context=ctx)
            else:
                conn = http.client.HTTPConnection(p.hostname, port, timeout=SOCKET_TIMEOUT)
            conn.sock = sock
            target = urllib.parse.urlunsplit(('', '', p.path or '/', p.query, ''))
            sock.settimeout(min(SOCKET_TIMEOUT, _remaining(deadline, clock)))
            conn.request('GET', target, headers={'User-Agent': 'waybar-notch-art', 'Accept-Encoding': 'identity',
                                                 'Connection': 'close'})
            resp = conn.getresponse()
        except (OSError, http.client.HTTPException, ssl.SSLError):
            try:
                sock.close()
            except OSError:
                pass
            raise ArtError('request failed') from None
        if resp.status in (301, 302, 303, 307, 308):
            loc = resp.getheader('Location')
            conn.close()
            if not loc:
                raise ArtError('redirect without location')
            previous = p
            url = urllib.parse.urljoin(url, loc)
            continue
        if 200 <= resp.status < 300:
            return conn, resp
        conn.close()
        raise ArtError('unexpected status')
    raise ArtError('too many redirects')


def download(url, dir_fd, tmp_name, deadline, resolver=socket.getaddrinfo, connect=default_connect,
             tls_context=None, clock=time.monotonic, max_bytes=MAX_BYTES):
    """Stream the body into a new 0600 exclusive file inside dir_fd; return the byte count."""
    conn, resp = open_response(url, deadline, resolver, connect, tls_context, clock)
    fd = None
    try:
        fd = os.open(tmp_name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600,
                     dir_fd=dir_fd)
        total = 0
        while True:
            _remaining(deadline, clock)
            try:
                chunk = resp.read(CHUNK)
            except (OSError, http.client.HTTPException):
                raise ArtError('read failed') from None
            if not chunk:
                break
            total += len(chunk)
            if total > max_bytes:
                raise ArtError('response too large')
            view = memoryview(chunk)
            while view:
                view = view[os.write(fd, view):]
        if total == 0:
            raise ArtError('empty response')
        os.fsync(fd)
        return total
    finally:
        conn.close()
        if fd is not None:
            os.close(fd)


def open_dir(path):
    return os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)


def owned_regular(name, dir_fd):
    """True when `name` in dir_fd is a regular file owned by us (never follows a link)."""
    try:
        fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC, dir_fd=dir_fd)
    except OSError:
        return False
    try:
        st = os.fstat(fd)
    finally:
        os.close(fd)
    return stat.S_ISREG(st.st_mode) and st.st_uid == os.getuid()


def prune(dir_fd, keep):
    for name in os.listdir(dir_fd):
        if name == keep or not CACHE_RE.match(name):
            continue
        try:
            st = os.lstat(name, dir_fd=dir_fd)
            if stat.S_ISREG(st.st_mode) and st.st_uid == os.getuid():
                os.unlink(name, dir_fd=dir_fd)
        except OSError:
            pass


def fetch_cover(rt, url, resolver=socket.getaddrinfo, connect=default_connect, tls_context=None,
                clock=time.monotonic, max_bytes=MAX_BYTES, total=TOTAL_DEADLINE):
    """Return the cached cover path for a remote URL, downloading when needed. Raises ArtError."""
    name = cache_name(url)
    dir_fd = open_dir(rt)
    tmp = None
    try:
        if owned_regular(name, dir_fd):
            return os.path.join(rt, name)
        tmp = '.tmp-' + PREFIX + secrets.token_hex(8)
        try:
            download(url, dir_fd, tmp, clock() + total, resolver, connect, tls_context, clock, max_bytes)
            os.replace(tmp, name, src_dir_fd=dir_fd, dst_dir_fd=dir_fd)
        except OSError:
            raise ArtError('cannot publish cover') from None
        tmp = None
        prune(dir_fd, name)
        return os.path.join(rt, name)
    finally:
        if tmp:
            try:
                os.unlink(tmp, dir_fd=dir_fd)
            except OSError:
                pass
        os.close(dir_fd)


def local_cover(url, max_bytes=MAX_BYTES):
    """Resolve a file: URI (empty/localhost authority) to an owned bounded regular file path."""
    p = urllib.parse.urlsplit(url)
    if p.netloc not in ('', 'localhost') or '\0' in url:
        raise ArtError('unsupported file URI')
    path = urllib.parse.unquote(p.path)
    if '\0' in path or not os.path.isabs(path):
        raise ArtError('unsupported file URI')
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC)
    except OSError:
        raise ArtError('cover is not readable') from None
    try:
        st = os.fstat(fd)
    finally:
        os.close(fd)
    if not stat.S_ISREG(st.st_mode) or st.st_uid != os.getuid() or not 0 < st.st_size <= max_bytes:
        raise ArtError('cover is not an acceptable file')
    return path


def current_url():
    try:
        return subprocess.run(['playerctl', '-p', PLAYERS, 'metadata', 'mpris:artUrl'],
                              capture_output=True, text=True, timeout=3).stdout.strip()
    except Exception:
        return ''


def main():
    try:
        rt = plugin().runtime_dir()
    except Exception:
        print('media-art: runtime directory is not trustworthy', file=sys.stderr)
        return 1
    url = current_url()
    if not url:
        return 0
    try:
        path = local_cover(url) if url.startswith('file:') else fetch_cover(rt, url)
    except ArtError:
        return 0
    print(path)
    return 0


if __name__ == '__main__':
    sys.exit(main())
