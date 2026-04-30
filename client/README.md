# drua

CLI for Drua.

## Quick start

```bash
cargo run -p drua-cli -- tui
```

This will:
1. Open your browser to authenticate via GitHub OAuth
2. Prompt you to paste the generated API token
3. Launch the interactive TUI tui

If you're already logged in, it skips straight to the tui.

## Commands

| Command | Description |
|---------|-------------|
| `drua tui` | Interactive TUI (auto-triggers login if needed) |
| `drua login` | Authenticate with a drua server |
| `drua logout` | Remove stored credentials |
| `drua status` | Show current connection status |
| `drua project list` | List all projects |
| `drua project create <name>` | Create a new project |
| `drua project show <id>` | Show project details |

## Dashboard keys

### Sidebar (default focus)
- `↑/↓` or `j/k` — navigate projects
- `n` — create new project
- `r` — refresh project list
- `Tab` — switch to chat pane
- `q` — quit

### Chat pane
- Type to compose a message to the project lead agent
- `Enter` — send message
- `↑/↓` — scroll chat history
- `Esc` or `Tab` — return to sidebar

## Options

```
--server <URL>   Server URL (default: http://localhost:4200)
                 Also configurable via DRUA_SERVER_URL env var
```

## Config

Credentials are stored in `~/.drua/config.json`.
