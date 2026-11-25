-- >>> waybar-pill-bar BEGIN (managed block, do not edit) >>>
-- The notch and pills (one waybar-notch process) are owned by the Omarchy plugin backend (waybar.pill-bar), not by systemd units.
-- Notch and pills: translucent glass with compositor blur behind them; OSD and notifications stay opaque (global blur preference untouched)
hl.layer_rule({ match = { namespace = "^waybar-notch$" }, blur = true, ignore_alpha = 0.1 })
hl.layer_rule({ match = { namespace = "^(waybar-osd|waybar-notifications)$" }, blur = false })
-- <<< waybar-pill-bar END <<<
