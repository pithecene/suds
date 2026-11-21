#!/bin/sh
# install.sh [binary]: installs the window, its desktop entry, and its icon for this user.
# Without a binary, the script installs the one of the release archive that holds it.
set -eu

here=$(dirname "$0")
source="${1:-$here/../../../bin/suds-desktop}"
if [ ! -f "$source" ]; then
    echo "install.sh: no binary at $source. Name the binary to install." >&2
    exit 1
fi
binary="$HOME/.local/bin/suds-desktop"
# A desktop reads the user's data directory, and the home's default serves only without one.
data="${XDG_DATA_HOME:-$HOME/.local/share}"

install -D -m 755 "$source" "$binary"
install -D -m 644 "$here/assets/suds.svg" "$data/icons/hicolor/scalable/apps/suds.svg"

# A desktop's search path may lack the binary's directory, so the entry names the full path.
# The entry's rules quote the path and escape four characters inside the quotes.
escaped=$(printf '%s' "$binary" | sed 's/[\\"`$]/\\&/g')
mkdir -p "$data/applications"
while IFS= read -r line; do
    case "$line" in
    Exec=*) printf 'Exec="%s"\n' "$escaped" ;;
    *) printf '%s\n' "$line" ;;
    esac
done <"$here/assets/suds.desktop" >"$data/applications/suds.desktop"
