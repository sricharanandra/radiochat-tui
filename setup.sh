#!/bin/bash
# RadioChat TUI Client Setup Script
# Usage: ./setup.sh <username> <jwt-token>

set -e

if [ "$#" -ne 2 ]; then
    echo "Usage: $0 <username> <jwt-token>"
    echo "Example: $0 alice eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9..."
    exit 1
fi

USERNAME="$1"
JWT_TOKEN="$2"

echo "🚀 Setting up RadioChat for user: $USERNAME"

# Create config directory
mkdir -p ~/.config/radiochat
echo "✅ Created config directory"

# Create config file
cat > ~/.config/radiochat/config.toml << EOF
[server]
url = "wss://radiochat.sreus.tech/ws"

[auth]
token_path = "~/.config/radiochat/token"

[ui]
show_timestamps = true
message_limit = 1000
multiline_mode = false

[network]
reconnect_attempts = 10
ping_interval = 30
EOF
echo "✅ Created config file"

# Save JWT token
echo "$JWT_TOKEN" > ~/.config/radiochat/token
chmod 600 ~/.config/radiochat/token
echo "✅ Saved authentication token"

echo ""
echo "🎉 Setup complete!"
echo ""
echo "To start RadioChat:"
echo "  cargo run --release"
echo ""
echo "Or build once and run the binary:"
echo "  cargo build --release"
echo "  ./target/release/radiochat"
echo ""
echo "Controls:"
echo "  • Vim keybindings: hjkl, i/a/o for insert, Esc for normal mode"
echo "  • Ctrl+Esc: Quit (with confirmation)"
echo "  • Ctrl+Shift+C/V: Copy/Paste"
echo "  • Mouse: Scroll and select text"
echo ""
