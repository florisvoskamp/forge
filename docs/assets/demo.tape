# vhs tape for the README hero GIF.  Render:  vhs docs/assets/demo.tape
# Docs: https://github.com/charmbracelet/vhs
Output docs/assets/demo.gif

Set FontSize 22
Set Width 1640
Set Height 920
Set Theme "Catppuccin Mocha"
Set Padding 24
Set Framerate 30

# A model-agnostic one-shot task: Forge classifies, routes, and edits.
Type "forge run 'add a --json flag to the status command'"
Sleep 600ms
Enter
Sleep 8s

# Show the mesh routing decision for a task without running it.
Type "forge mesh 'refactor the auth middleware'"
Sleep 400ms
Enter
Sleep 4s

# Code-graph blast radius.
Type "forge lattice impact UserRepository"
Sleep 400ms
Enter
Sleep 4s
