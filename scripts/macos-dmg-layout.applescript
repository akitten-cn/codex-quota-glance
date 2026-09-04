on run argv
    set volumeName to item 1 of argv
    tell application "Finder"
        tell disk volumeName
            open
            set current view of container window to icon view
            set toolbar visible of container window to false
            set statusbar visible of container window to false
            set bounds of container window to {160, 120, 800, 550}
            set options to icon view options of container window
            set arrangement of options to not arranged
            set icon size of options to 104
            set text size of options to 14
            set background picture of options to file ".background:installer-background.png"
            set position of item "Codex Taskbar.app" to {170, 195}
            set position of item "Applications" to {470, 195}
            set extension hidden of item "Codex Taskbar.app" to true
            update without registering applications
            delay 2
            close
            open
        end tell
        activate
    end tell
end run
