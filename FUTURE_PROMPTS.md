1. Output capture in notifications — When a task exits or completes, you send
  the status but not the last N lines of stdout/stderr. You'd need to know
what the agent actually did/said.
2. Task chaining/dependencies — No way to say "start task B when task A
completes," which would be natural for agent pipelines.
3. Sandbox exit notifications — Containers can be killed but there's no
Telegram alert sent on container exit.
-####

- The agent-inbox keeps the status around when claude code or opencode haven't been gracefully killed. I think we should get a cron that verifies if the PID is still running and kills it if not to avoid polluting the database.
- Implement Telegram notifications for gemini and claude in the web?
