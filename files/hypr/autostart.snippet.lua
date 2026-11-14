-- >>> waybar-pill-bar BEGIN (managed block, do not edit) >>>
-- Waybar and the notch are owned by the Omarchy plugin backend (waybar.pill-bar), not by systemd units.
-- Frosted glass: real compositor blur behind the bar, notch, popups, OSD and notifications (never the click-catcher)
hl.config({ decoration = { blur = { enabled = true } } })
hl.layer_rule({ match = { namespace = "^(waybar|waybar-notch|waybar-popup|waybar-osd|waybar-notifications)$" }, blur = true, ignore_alpha = 0.03 })
-- Popup animation: pop in; the invisible click-catcher stays instant
hl.layer_rule({ match = { namespace = "waybar-popup" }, animation = "popin 88%" })
hl.layer_rule({ match = { namespace = "waybar-popup-catcher" }, no_anim = true, animation = "none" })
-- <<< waybar-pill-bar END <<<
