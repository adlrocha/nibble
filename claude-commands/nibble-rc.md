---
description: Continue this session from Slack (nibble rc). `/nibble-rc stop` takes it back.
argument-hint: "[stop]"
allowed-tools: Bash(nibble rc start:*), Bash(nibble rc stop:*)
---
<!-- nibble's UserPromptSubmit hook normally handles /nibble-rc before this
     prompt is expanded, so no model turn is spent. This body only runs when
     that hook is not installed (re-run ./install.sh to install it). -->
If the argument "$ARGUMENTS" is "stop" or "off", run `nibble rc stop`; otherwise run `nibble rc start`. Reply with the command's output only.
