# Codelatch

Telegram supervision broker for Claude Code.

Start Claude in a managed tmux session, walk away, and handle questions/approvals from Telegram.

## Install

```bash
cargo install codelatch
```

## First Run

```bash
codelatch
```

On first run, Codelatch will:

1. Prompt for your Telegram bot token
2. Pair to your chat via `/start`
3. Install Claude hooks
4. Start the daemon
5. Launch the first managed Claude session

## Common Commands

```bash
# launch managed Claude session (default command)
codelatch

# daemon health
codelatch status
codelatch doctor
codelatch doctor --fix

# daemon lifecycle
codelatch start
codelatch stop

# install as user service (launchd/systemd)
codelatch service install
codelatch service status
codelatch service uninstall
```

## Telegram Commands

- `/peek` - current task, running command, recent terminal output, and inline actions
- `/diff` - current git diff (as inline text or patch attachment)
- `/log` - last 200 lines of tmux output as attachment
- `/sessions` - list tracked sessions
- `/switch <name>` - set default session for freeform messages

## Troubleshooting

- Run `codelatch doctor --fix` for automatic recovery.
- Ensure `tmux` is installed and available on PATH.
- If Telegram auth fails, rerun `codelatch init`.
