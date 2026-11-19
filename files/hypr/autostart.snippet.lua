-- >>> waybar-pill-bar BEGIN (managed block, do not edit) >>>
-- The notch and pills (one waybar-notch process) are owned by the Omarchy plugin backend (waybar.pill-bar), not by systemd units.
-- Opaque theme-colored panels: no compositor blur behind the notch, OSD and notifications (global blur preference untouched)
hl.layer_rule({ match = { namespace = "^(waybar-notch|waybar-osd|waybar-notifications)$" }, blur = false })
-- <<< waybar-pill-bar END <<<
