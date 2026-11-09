#!/usr/bin/env bash
# Устанавливает иконку и ярлык в меню приложений, чтобы «Библиотека FB2»
# находилась поиском в лаунчере.
#
#   ./install.sh            установить
#   ./install.sh --remove   удалить ярлык и иконки
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP_ID="library_gui"
BIN="$HERE/target/release/library_gui"
DATA_DIR="$HERE/data"

ICON_BASE="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor"
APPS_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
DESKTOP="$APPS_DIR/$APP_ID.desktop"
SIZES=(16 24 32 48 64 128 256 512)

# --- удаление ---------------------------------------------------------------
if [[ "${1:-}" == "--remove" ]]; then
    rm -f "$DESKTOP"
    for s in "${SIZES[@]}"; do rm -f "$ICON_BASE/${s}x${s}/apps/$APP_ID.png"; done
    command -v update-desktop-database >/dev/null && update-desktop-database "$APPS_DIR" 2>/dev/null || true
    command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -f -t "$ICON_BASE" 2>/dev/null || true
    echo "Удалено: $DESKTOP и иконки $APP_ID"
    exit 0
fi

# --- проверки ---------------------------------------------------------------
if [[ ! -x "$BIN" ]]; then
    echo "Бинарника нет — собираю..."
    ( cd "$HERE" && cargo build --release )
fi
[[ -x "$BIN" ]] || { echo "Ошибка: не найден $BIN" >&2; exit 1; }

if ! command -v rsvg-convert >/dev/null; then
    echo "Ошибка: нужен rsvg-convert (пакет librsvg) для рендера иконок" >&2
    exit 1
fi

mkdir -p "$APPS_DIR"

# --- иконки во всех размерах ------------------------------------------------
for s in "${SIZES[@]}"; do
    dir="$ICON_BASE/${s}x${s}/apps"
    mkdir -p "$dir"
    rsvg-convert -w "$s" -h "$s" "$HERE/assets/icon.svg" -o "$dir/$APP_ID.png"
done
echo "Иконки: $ICON_BASE/<размер>/apps/$APP_ID.png (${SIZES[*]})"

# --- ярлык ------------------------------------------------------------------
cat > "$DESKTOP" <<EOF
[Desktop Entry]
Type=Application
Version=1.1
Name=Библиотека FB2
Name[en]=FB2 Library
GenericName=Библиотека и читалка FB2/EPUB
Comment=Локальная библиотека FB2/EPUB: поиск, обложки, упаковка в ARX
Exec=$BIN $DATA_DIR
TryExec=$BIN
Icon=$APP_ID
Terminal=false
Categories=Office;Viewer;
Keywords=библиотека;книги;fb2;epub;читалка;поиск;library;books;epub;fb2;reader;arx;
StartupNotify=true
StartupWMClass=$APP_ID
EOF
chmod 644 "$DESKTOP"
echo "Ярлык:  $DESKTOP"

# --- обновить кэши ----------------------------------------------------------
command -v desktop-file-validate >/dev/null && desktop-file-validate "$DESKTOP" || true
command -v update-desktop-database >/dev/null && update-desktop-database "$APPS_DIR" 2>/dev/null || true
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -f -t "$ICON_BASE" 2>/dev/null || true

echo
echo "Готово. Найди в лаунчере: «Библиотека» или «FB2»."
