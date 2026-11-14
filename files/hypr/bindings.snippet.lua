-- >>> waybar-pill-bar BEGIN (managed block, do not edit) >>>
-- Super+Shift+Space shows/hides Waybar (replaces Omarchy's "toggle top bar")
hl.unbind("SUPER + SHIFT + SPACE")
o.bind("SUPER + SHIFT + SPACE", "Toggle Waybar", "systemctl --user kill --kill-whom=main -s SIGUSR1 waybar.service")
-- Brightness keys show the notch-style overlay (volume keys already do via waybar-volume-osd.py)
for _, k in ipairs({ "XF86MonBrightnessUp", "XF86MonBrightnessDown", "SHIFT + XF86MonBrightnessUp", "SHIFT + XF86MonBrightnessDown",
                     "ALT + XF86MonBrightnessUp", "ALT + XF86MonBrightnessDown" }) do hl.unbind(k) end
o.bind("XF86MonBrightnessUp", "Brightness up", "__HOME__/.config/waybar/scripts/waybar-brightness-key +5%", { locked = true, repeating = true })
o.bind("XF86MonBrightnessDown", "Brightness down", "__HOME__/.config/waybar/scripts/waybar-brightness-key 5%-", { locked = true, repeating = true })
o.bind("SHIFT + XF86MonBrightnessUp", "Brightness maximum", "__HOME__/.config/waybar/scripts/waybar-brightness-key 100%", { locked = true })
o.bind("SHIFT + XF86MonBrightnessDown", "Brightness minimum", "__HOME__/.config/waybar/scripts/waybar-brightness-key 1%", { locked = true })
o.bind("ALT + XF86MonBrightnessUp", "Brightness up precise", "__HOME__/.config/waybar/scripts/waybar-brightness-key +1%", { locked = true, repeating = true })
o.bind("ALT + XF86MonBrightnessDown", "Brightness down precise", "__HOME__/.config/waybar/scripts/waybar-brightness-key 1%-", { locked = true, repeating = true })
-- <<< waybar-pill-bar END <<<
