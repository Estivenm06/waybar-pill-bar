-- >>> waybar-pill-bar BEGIN (managed block, do not edit) >>>
-- Plugin mode: keys work whether or not the plugin is enabled; the helpers decide (native Omarchy behavior when inactive).
hl.unbind("SUPER + SHIFT + SPACE")
o.bind("SUPER + SHIFT + SPACE", "Toggle bar", "__HOME__/.config/waybar/scripts/waybar-pill-plugin toggle")
for _, k in ipairs({ "XF86AudioRaiseVolume", "XF86AudioLowerVolume", "XF86AudioMute",
                     "XF86MonBrightnessUp", "XF86MonBrightnessDown", "SHIFT + XF86MonBrightnessUp", "SHIFT + XF86MonBrightnessDown",
                     "ALT + XF86MonBrightnessUp", "ALT + XF86MonBrightnessDown" }) do hl.unbind(k) end
o.bind("XF86AudioRaiseVolume", "Volume up", "/usr/bin/python3 __HOME__/.config/waybar/scripts/waybar-volume-osd.py raise", { locked = true, repeating = true })
o.bind("XF86AudioLowerVolume", "Volume down", "/usr/bin/python3 __HOME__/.config/waybar/scripts/waybar-volume-osd.py lower", { locked = true, repeating = true })
o.bind("XF86AudioMute", "Mute toggle", "/usr/bin/python3 __HOME__/.config/waybar/scripts/waybar-volume-osd.py mute-toggle", { locked = true })
o.bind("XF86MonBrightnessUp", "Brightness up", "__HOME__/.config/waybar/scripts/waybar-brightness-key +5%", { locked = true, repeating = true })
o.bind("XF86MonBrightnessDown", "Brightness down", "__HOME__/.config/waybar/scripts/waybar-brightness-key 5%-", { locked = true, repeating = true })
o.bind("SHIFT + XF86MonBrightnessUp", "Brightness maximum", "__HOME__/.config/waybar/scripts/waybar-brightness-key 100%", { locked = true })
o.bind("SHIFT + XF86MonBrightnessDown", "Brightness minimum", "__HOME__/.config/waybar/scripts/waybar-brightness-key 1%", { locked = true })
o.bind("ALT + XF86MonBrightnessUp", "Brightness up precise", "__HOME__/.config/waybar/scripts/waybar-brightness-key +1%", { locked = true, repeating = true })
o.bind("ALT + XF86MonBrightnessDown", "Brightness down precise", "__HOME__/.config/waybar/scripts/waybar-brightness-key 1%-", { locked = true, repeating = true })
-- <<< waybar-pill-bar END <<<
