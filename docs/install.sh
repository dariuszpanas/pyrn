#!/bin/sh
# Install the standalone pyrn release executable or an explicitly selected Python tool environment.
set -eu

die() { printf 'pyrn installer: %s\n' "$*" >&2; exit 2; }

install_python() (
    package=pyrn
    [ -z "$version" ] || package="pyrn==$version"
    if command -v uv >/dev/null 2>&1; then
        uv_bin=$(command -v uv)
    else
        [ -n "${HOME:-}" ] || die 'installing uv requires HOME; set HOME or install uv on PATH'
        installer=$(mktemp)
        trap 'rm -f "$installer"' EXIT HUP INT TERM
        if command -v curl >/dev/null 2>&1; then
            curl --fail --silent --show-error --location --proto '=https' --proto-redir '=https' \
                --connect-timeout 15 --max-time 120 \
                https://astral.sh/uv/install.sh --output "$installer"
        elif command -v wget >/dev/null 2>&1; then
            wget --https-only --timeout=120 --tries=1 \
                --output-document "$installer" https://astral.sh/uv/install.sh
        else
            die 'installing uv requires curl or wget; install either tool or uv, then retry'
        fi
        UV_UNMANAGED_INSTALL="$HOME/.local/bin" sh "$installer"
        uv_bin="$HOME/.local/bin/uv"
    fi
    "$uv_bin" --no-config tool install --python 3.12 "$package"
    case "$(uname -s)" in
        MSYS*|MINGW*|CYGWIN*)
            tool_bin=$("$uv_bin" --no-config tool dir --bin)
            shell_bin=$(cygpath -u "$tool_bin" | sed "s/'/'\\\\''/g")
            shell_uv_bin=$(cygpath -u "$(dirname "$uv_bin")" | sed "s/'/'\\\\''/g")
            printf '%s\n' 'pyrn installed. In MSYS2, Git Bash, or Cygwin, run in your current shell:'
            printf "  export PATH='%s':'%s':\"\$PATH\"\n" "$shell_bin" "$shell_uv_bin"
            printf '%s\n' \
                '  hash -r' \
                '  pyrn --help' \
                'Add the export line to ~/.zshrc (zsh) or ~/.bashrc (bash) to keep it for new shells.'
            ;;
        *)
            if [ "${PYRN_NO_MODIFY_PATH:-0}" != 1 ]; then
                "$uv_bin" --no-config tool update-shell
            fi
            printf '%s\n' 'pyrn installed. Open a new terminal and run pyrn --help.'
            ;;
    esac
    printf '%s\n' \
        'Update: uv tool upgrade pyrn. Remove: uv tool uninstall pyrn.'
)

valid_version() {
    case "$1" in ''|*[!0-9.]*) return 1 ;; esac
    printf '%s\n' "$1" | grep -Eq '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
}

# Read one Location header from the last HTTP response. GNU Wget indents response headers
# and separately echoes "Location: ... [following]" as a progress message.
redirect_url() {
    next=$(LC_ALL=C awk -v client="$2" '
        client=="wget" && !sub(/^  /, "") {next}
        {sub(/\r$/, "")}
        /^HTTP\/[0-9.]+[ \t]/ {status=$2; count=0; folded=0; header=0; next}
        /^[ \t]/ {if (header) folded=1; next}
        {header=(tolower(substr($0, 1, 9))=="location:")}
        header {value=substr($0, 10); sub(/^[ \t]+/, "", value); sub(/[ \t]+$/, "", value); count++}
        END {
            if (status!~/^(301|302|303|307|308)$/ || count!=1 || folded ||
                !length(value) || value ~ /[[:space:][:cntrl:]\\]/) exit 1
            print value
        }
    ' "$tmp/headers") || return 1
    request=${1%%\#*}; path_url=${request%%\?*}
    after_scheme=${path_url#*://}; origin="${path_url%%://*}://${after_scheme%%/*}"
    case "$next" in
        https://*|http://*) authority=${next#*://}; authority=${authority%%[/?#]*}
            case "$authority" in ''|*@*) return 1 ;; esac
            printf '%s\n' "$next" ;;
        //*) return 1 ;;
        /*) printf '%s%s\n' "$origin" "$next" ;;
        \?*) printf '%s%s\n' "$path_url" "$next" ;;
        \#*) printf '%s%s\n' "$request" "$next" ;;
        *) case "${next%%[/?#]*}" in *:*) return 1 ;; esac
            path=${path_url#"$origin"}; parent=${path%/*}
            while :; do
                case "$next" in ./*) next=${next#./} ;; ../*) parent=${parent%/*}; next=${next#../} ;; *) break ;; esac
            done
            printf '%s%s/%s\n' "$origin" "$parent" "$next" ;;
    esac
}

# Status 44 means an actual HTTP 404. All other failures end the installation.
fetch() {
    url=$1 out=$2 limit=$3
    case "$base" in
        http:*) protocols='=http'; redirects=0 ;;
        *) protocols='=https'; redirects=5 ;;
    esac
    if command -v curl >/dev/null 2>&1; then
        status=$(curl -q --silent --show-error --location --max-redirs "$redirects" \
            --proto "$protocols" --proto-redir "$protocols" --connect-timeout 15 --max-time 120 \
            --max-filesize "$limit" --output "$out" --write-out '%{http_code}' "$url") || return 1
    elif command -v wget >/dev/null 2>&1; then
        while :; do
            (ulimit -f "$((limit / 512 + 1))"; LC_ALL=C wget --no-config --max-redirect=0 --server-response --timeout=120 --tries=1 \
                --output-document "$out" "$url") 2>"$tmp/headers" && break
            status=$(awk '/^  HTTP\// {s=$2} END {print s}' "$tmp/headers")
            case "$status" in
                404) return 44 ;;
                301|302|303|307|308)
                    [ "$redirects" -gt 0 ] || return 1
                    url=$(redirect_url "$url" wget) || return 1
                    case "$url" in https://*) ;; *) return 1 ;; esac
                    redirects=$((redirects - 1)) ;;
                *) return 1 ;;
            esac
        done
        status=$(awk '/^  HTTP\// {s=$2} END {print s}' "$tmp/headers")
    else
        die 'downloading a native release requires curl or wget'
    fi
    case "$status" in 200) ;; 404) return 44 ;; *) return 1 ;; esac
    [ "$(wc -c < "$out")" -le "$limit" ]
}

latest_tag() {
    case "$base" in
        http:*) protocols='=http' ;;
        *) protocols='=https' ;;
    esac
    if command -v curl >/dev/null 2>&1; then
        client=curl
        curl -q --silent --show-error --head --proto "$protocols" --connect-timeout 15 --max-time 30 \
            "$base/latest" > "$tmp/headers" || return 1
    elif command -v wget >/dev/null 2>&1; then
        client=wget
        LC_ALL=C wget --no-config --spider --server-response --max-redirect=0 --timeout=30 --tries=1 \
            "$base/latest" 2>"$tmp/headers" || :
    else
        return 1
    fi
    tag=$(redirect_url "$base/latest" "$client") || return 1
    case "$tag" in "$base"/tag/v*) candidate=${tag#"$base"/tag/v} ;; *) return 1 ;; esac
    valid_version "$candidate" && printf '%s\n' "$candidate"
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sum=$(sha256sum < "$1") || return 1
    elif command -v shasum >/dev/null 2>&1; then
        sum=$(shasum -a 256 < "$1") || return 1
    else
        die 'verification requires sha256sum or shasum'
    fi
    printf '%s\n' "${sum%% *}"
}

extract_binary() {
    archive_path=$1
    out_exe=$2
    if [ "$ext" = tar.gz ]; then
        command -v tar >/dev/null 2>&1 || die 'extracting tar.gz releases requires tar'
        tar -xzf "$archive_path" -C "$tmp/extract" "$exe" || die 'could not extract pyrn from archive'
        [ -f "$tmp/extract/$exe" ] && [ ! -L "$tmp/extract/$exe" ] || die 'archive missing regular pyrn executable'
        mv -f "$tmp/extract/$exe" "$out_exe"
    else
        if command -v unzip >/dev/null 2>&1; then
            unzip -q -o -j "$archive_path" "$exe" -d "$tmp/extract" || die 'could not extract pyrn.exe from zip'
            [ -f "$tmp/extract/$exe" ] && [ ! -L "$tmp/extract/$exe" ] || die 'zip missing regular pyrn.exe executable'
            mv -f "$tmp/extract/$exe" "$out_exe"
        elif command -v python3 >/dev/null 2>&1 || command -v python >/dev/null 2>&1; then
            py_cmd=$(command -v python3 2>/dev/null || command -v python)
            "$py_cmd" -c '
import sys, zipfile
with zipfile.ZipFile(sys.argv[1]) as zf:
    info = zf.getinfo(sys.argv[2])
    if info.is_dir():
        raise SystemExit(1)
    with open(sys.argv[3], "wb") as dst:
        dst.write(zf.read(info))
' "$archive_path" "$exe" "$out_exe" || die 'could not extract pyrn.exe from zip'
        else
            die 'extracting zip releases requires unzip or python'
        fi
    fi
}

install_pyrn() (
    method=${PYRN_INSTALL_METHOD:-auto}
    version=${PYRN_VERSION:-}
    dir=${PYRN_INSTALL_DIR:-}
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --python) method=python ;;
            --native) method=native ;;
            --version|--install-dir)
                [ "$#" -ge 2 ] && [ -n "$2" ] || die "$1 requires a value"
                if [ "$1" = --version ]; then version=$2; else dir=$2; fi
                shift ;;
            --help|-h) echo 'Usage: install.sh [--native|--python] [--version X.Y.Z] [--install-dir /absolute/path]'; exit ;;
            *) die 'unknown installer argument; see --help' ;;
        esac
        shift
    done
    if [ -n "$version" ]; then valid_version "$version" || die 'PYRN_VERSION must be a final X.Y.Z release'; fi
    case "$method" in python) install_python; exit ;; native|auto) ;; *) die 'unknown PYRN_INSTALL_METHOD' ;; esac
    if [ -z "$dir" ]; then
        [ -n "${HOME:-}" ] || die 'set --install-dir or PYRN_INSTALL_DIR when HOME is unavailable'
        dir="$HOME/.local/bin"
    fi
    case "$(uname -s)" in
        Linux) os=unknown-linux-musl; ext=tar.gz; exe=pyrn ;;
        Darwin) os=apple-darwin; ext=tar.gz; exe=pyrn ;;
        MSYS*|MINGW*|CYGWIN*) os=pc-windows-msvc; ext=zip; exe=pyrn.exe ;;
        *) die 'no native release for this OS; select --python' ;;
    esac
    machine=$(uname -m)
    if [ "$os" = pc-windows-msvc ]; then machine=${PROCESSOR_ARCHITEW6432:-${PROCESSOR_ARCHITECTURE:-$machine}}; fi
    case "$machine" in x86_64|amd64|AMD64) arch=x86_64 ;; aarch64|arm64|ARM64) arch=aarch64 ;; *) die 'no native release for this architecture; select --python' ;; esac
    base=${PYRN_RELEASE_BASE_URL:-https://github.com/dariuszpanas/pyrn/releases}
    case "$base" in *[!a-zA-Z0-9.:/_-]*) die 'invalid release URL' ;; esac
    printf '%s\n' "$base" | grep -Eq '^(https://[A-Za-z0-9.-]+(:[0-9]+)?(/[A-Za-z0-9._/-]*)?|http://127\.0\.0\.1:[0-9]+)$' || die 'release URL requires HTTPS or explicit loopback'
    case "$dir" in *'
'*|*"$(printf '\r')"*) die 'invalid installation directory' ;; esac
    case "$dir" in /*) ;; *) die '--install-dir requires an absolute path' ;; esac
    if ! command -v curl >/dev/null 2>&1; then
        command -v wget >/dev/null 2>&1 && wget --no-config --max-redirect=0 --version >/dev/null 2>&1 ||
            die 'native downloads require curl or GNU Wget with redirect controls'
    fi
    mkdir -p "$dir"
    tmp=$(mktemp -d "$dir/.pyrn-install.XXXXXXXX") || die 'cannot stage installation'
    mkdir -p "$tmp/extract"
    locked=0; publishing=0
    cleanup_install() {
        trap '' HUP INT TERM
        if [ "$publishing" = 1 ] && [ ! -e "$tmp/receipt" ] && [ -e "$tmp/$exe" ]; then
            if [ -f "$tmp/old-receipt" ]; then
                mv -f "$tmp/old-receipt" "$receipt" || die "could not restore install receipt; recovery files are in $tmp"
            else
                rm -f "$receipt" || die "could not remove incomplete install receipt in $dir"
            fi
        fi
        rm -rf "$tmp/extract"
        rm -f "$tmp/manifest" "$tmp/archive" "$tmp/$exe" "$tmp/headers" "$tmp/receipt" "$tmp/old-receipt"
        rmdir "$tmp" 2>/dev/null || :
        if [ "$locked" = 1 ]; then rmdir "$dir/.pyrn-native-lock" 2>/dev/null || :; fi
    }
    trap cleanup_install EXIT
    trap 'exit 2' HUP INT TERM

    release=$version
    if [ -z "$release" ]; then
        release=$(latest_tag) || die 'cannot identify the latest release; specify --version X.Y.Z or select --python'
    fi
    asset="pyrn-$release-$arch-$os.$ext"
    manifest_name="pyrn-v$release-sha256.txt"
    fetch "$base/download/v$release/$manifest_name" "$tmp/manifest" 65536 ||
        die "native release checksum manifest unavailable for v$release; for a Python installation select --python"

    expected=$(LC_ALL=C awk -v asset="$asset" '
        {
            sub(/\r$/, "")
            if (length($0) == 0) next
            hash = substr($0, 1, 64)
            sep = substr($0, 65, 2)
            name = substr($0, 67)
            if (hash !~ /^[0-9a-f]{64}$/ || sep != "  " || length(name) == 0) exit 1
            if (name == asset) {
                if (found) exit 1
                selected = hash
                found = 1
            }
        }
        END { if (!found) exit 1; print selected }
    ' "$tmp/manifest") || die "invalid release checksum manifest or missing entry for $asset"

    fetch "$base/download/v$release/$asset" "$tmp/archive" 134217728 || die "native archive download failed ($asset)"
    actual_archive=$(sha256 "$tmp/archive") || die 'archive checksum calculation failed'
    [ "$actual_archive" = "$expected" ] || die 'native archive checksum mismatch'

    extract_binary "$tmp/archive" "$tmp/$exe"
    chmod 755 "$tmp/$exe"
    ident=$("$tmp/$exe" --version) || die 'verified executable did not start'
    [ "${ident%"$(printf '\r')"}" = "pyrn $release" ] || die 'native executable version mismatch'
    actual_bin=$(sha256 "$tmp/$exe") || die 'executable checksum calculation failed'

    interrupted=0
    trap 'interrupted=1' HUP INT TERM
    (trap '' HUP INT TERM; exec mkdir "$dir/.pyrn-native-lock") || die 'another install is active; check .pyrn-native-lock'
    locked=1
    trap 'exit 2' HUP INT TERM
    [ "$interrupted" = 0 ] || exit 2

    dest="$dir/$exe"; receipt="$dir/.pyrn-native-receipt"
    [ ! -L "$dest" ] && [ ! -L "$receipt" ] || die 'refusing to overwrite a linked installation'
    if [ -e "$dest" ] || [ -e "$receipt" ]; then
        [ -f "$dest" ] && [ -f "$receipt" ] && [ "$(wc -c < "$receipt")" -le 256 ] || die 'existing executable belongs to another installer'
        IFS= read -r previous < "$receipt" || die 'invalid install receipt'
        previous=${previous#pyrn-native-install-v1 }
        valid_version "$previous" || die 'invalid install receipt'
        oldsum=$(sha256 "$dest") || die 'cannot verify installed executable'
        printf 'pyrn-native-install-v1 %s\n%s  %s\n' "$previous" "$oldsum" "$exe" > "$tmp/old-receipt"
        cmp -s "$tmp/old-receipt" "$receipt" || die 'installed executable or receipt has changed'
    fi

    printf 'pyrn-native-install-v1 %s\n%s  %s\n' "$release" "$actual_bin" "$exe" > "$tmp/receipt"
    publishing=1
    mv -f "$tmp/receipt" "$receipt"
    mv -f "$tmp/$exe" "$dest" || die 'could not replace installed executable'
    publishing=0
    printf 'Installed pyrn %s at %s\n' "$release" "$dest"

    case ":$PATH:" in *":$dir:"*) exit ;; esac
    quoted=$(printf '%s' "$dir" | sed "s/'/'\\\\''/g")
    export_path="export PATH='$quoted':\"\$PATH\""
    if [ "${PYRN_NO_MODIFY_PATH:-0}" != 1 ] && [ "$os" != pc-windows-msvc ]; then
        user_shell=${SHELL:-sh}
        case "${user_shell##*/}" in
            zsh) dotdir=${ZDOTDIR:-${HOME:-}}; profile=${dotdir:+"$dotdir/.zshrc"} ;;
            bash) profile=${HOME:+"$HOME/.bashrc"} ;;
            sh|dash|'') profile=${HOME:+"$HOME/.profile"} ;;
            *) profile='' ;;
        esac
        if [ -n "$profile" ] && [ ! -L "$profile" ] && { [ ! -e "$profile" ] || [ -f "$profile" ]; }; then
            if ! grep -Fqx -- "$export_path" "$profile" 2>/dev/null &&
               ! (printf '\n%s\n' "$export_path" >> "$profile") 2>/dev/null; then
                printf 'Could not update %s. Add the PATH command below to your shell profile.\n' "$profile" >&2
            fi
        fi
    fi
    printf 'To use pyrn in this shell, run:\n  %s\n' "$export_path"
)

install_pyrn "$@"
