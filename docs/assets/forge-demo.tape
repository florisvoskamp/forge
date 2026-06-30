# VHS tape for the Forge TUI demo GIF.  Render:  vhs docs/assets/forge-demo.tape
# Docs: https://github.com/charmbracelet/vhs
Output docs/assets/forge-demo.gif

# Look like a real, polished terminal window:
#   - JetBrainsMono Nerd Font for full glyph coverage (⚒ ↳ ✓ ● ⚠ ※)
#   - macOS-style title bar with traffic-light buttons
#   - rounded corners floating on a soft backdrop
Set FontFamily "JetBrainsMono Nerd Font"
Set FontSize 15
Set Theme "Catppuccin Mocha"
Set Width 1320
Set Height 840
Set Padding 26
Set Margin 48
Set MarginFill "#181825"
Set BorderRadius 12
Set WindowBar Colorful
Set WindowBarSize 36
Set Framerate 30
Set TypingSpeed 55ms

# Launch the full-screen TUI offline (mock mesh, no API key) off-camera.
Hide
Type "./target/debug/forge chat --mock"
Enter
Sleep 2500ms
Show

# A realistic coding request — Forge classifies, routes via the mesh,
# calls tools, and streams an answer.
Type "Add a retry-with-backoff wrapper around the HTTP client and a test for it"
Sleep 600ms
Enter
Sleep 9s

# Show the model picker overlay.
Type "/model"
Sleep 400ms
Enter
Sleep 3s
Escape
Sleep 1s

# Quit cleanly.
Ctrl+C
Sleep 1s
