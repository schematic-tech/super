#!/bin/sh
# Install: curl -fsSL https://get.schematic.tech/cli.sh | sh
set -eu

# Keep these in sync with the released CLI's minimum Rust version.
super_rust_major=1
super_rust_minor=97
super_rust_toolchain=1.97.1

interactive=false
cyan='' green='' red='' amber='' purple='' muted='' bold='' underline='' reset=''
if [ -t 2 ] && [ "${TERM:-dumb}" != dumb ]; then
    interactive=true
    if [ -z "${NO_COLOR:-}" ]; then
        # Match Super's output/style.rs palette and prompt/command emphasis.
        cyan=$(printf '\033[96m')
        green=$(printf '\033[38;2;149;212;173m')
        red=$(printf '\033[38;2;243;161;161m')
        amber=$(printf '\033[38;2;230;193;126m')
        purple=$(printf '\033[38;2;209;177;245m')
        muted=$(printf '\033[38;2;176;182;197m')
        bold=$(printf '\033[1m')
        underline=$(printf '\033[4m')
        reset=$(printf '\033[0m')
    fi
fi

spinner_pid=''
progress_file=''
progress_visible_file=''
progress_input=false
command_pid=''
tty_settings=''
install_log=''
skill_work=''
assume_yes=false
install_skills=true
install_phase=1
docs_origin=https://docs.schematic.tech
skill_origin=${SUPER_INSTALL_DOCS_ORIGIN:-$docs_origin}
skill_origin=${skill_origin%/}

terminal_columns() {
    terminal_width=80
    if terminal_size=$(stty size 2>/dev/null </dev/tty); then
        terminal_size=${terminal_size##* }
        case "$terminal_size" in
            ''|*[!0-9]*) ;;
            *) if [ "$terminal_size" -ge 20 ]; then terminal_width=$terminal_size; fi ;;
        esac
    fi
}

stop_progress() {
    if [ -n "$progress_file" ]; then rm -f "$progress_file"; fi
    if [ -n "$spinner_pid" ]; then
        wait "$spinner_pid" 2>/dev/null || :
        spinner_pid=''
    fi
    progress_file=''
    # Reuse the overview between install stages without a blank frame.
    if [ "${1:-false}" = true ]; then return; fi
    # The renderer marks visibility only when it draws. Fast skill downloads
    # never reserve a row, hide the cursor, or leave a transient loading message.
    if [ -n "$progress_visible_file" ] && [ -f "$progress_visible_file" ]; then
        printf '\r\033[K' >&2
        remaining_rows=$((progress_rows - 1))
        while [ "$remaining_rows" -gt 0 ]; do
            printf '\033[1A\r\033[K' >&2
            remaining_rows=$((remaining_rows - 1))
        done
        printf '\033[?25h' >&2
        rm -f "$progress_visible_file"
    fi
    progress_visible_file=''
    if [ "$progress_input" = true ]; then
        discard_progress_input
        restore_tty
        progress_input=false
    fi
}

discard_progress_input() {
    # Loading has no input controls except the terminal's normal interrupt keys.
    # Discard stray keys so Enter cannot move the display or answer the later prompt.
    if [ "$progress_input" = true ]; then
        dd if=/dev/tty of=/dev/null bs=256 count=1 2>/dev/null || :
    fi
}

show_progress() {
    : >"$progress_visible_file"
    if [ "$progress_mode" = skill ]; then
        printf '\033[?25l' >&2
    else
        # A fixed six-row overview keeps all three stages visible.
        printf '\033[?25l\n\n\n\n\n' >&2
    fi
}

progress_frame() {
    discard_progress_input
    terminal_columns
    # Cargo exposes actual completed/total build jobs with term.progress.when=always.
    # Rustup exposes per-component download/unpack bars. Read their bounded latest
    # output; unrecognized formats fall back to activity, never invented percentages.
    # The bounded tail can start inside a UTF-8 character (Cargo uses ellipses).
    # Byte mode must cover tr too: macOS tr otherwise emits "Illegal byte
    # sequence" into the live terminal, moving the cursor and duplicating rows.
    tail -c 16384 "$install_log" | LC_ALL=C tr '\r' '\n' | LC_ALL=C awk \
        -v label="$progress_label" -v mode="$progress_mode" -v phase="$install_phase" \
        -v tick="$1" -v skills="$install_skills" -v width="$terminal_width" \
        -v activity_file="${install_log}.activity" \
        -v accent="$purple" -v muted="$muted" -v pass="$green" -v bold="$bold" -v reset="$reset" '
        function stage(text) { label=text; done=0; total=0; metric="" }
        {
            gsub(/\033\[[0-9;?]*[ -\/]*[@-~]/, "")
            sub(/^[[:space:]]+/, "")
        }
        /^Building[[:space:]]+\[/ || /^Downloading[[:space:]]+\[/ {
            text=$0
            sub(/^.*\][[:space:]]*/, "", text)
            sub(/:.*/, "", text)
            if (text ~ /^[0-9]+\/[0-9]+$/) {
                split(text, jobs, "/")
                stage($1 == "Building" ? "Building Super CLI" : "Downloading dependencies")
                if (jobs[2] > 0 && jobs[1] <= jobs[2]) {
                    done=jobs[1]; total=jobs[2]
                    metric=sprintf("%d%%", 100*done/total)
                }
            }
            next
        }
        /^Compiling / { if (label != "Building Super CLI") stage("Building Super CLI"); next }
        /^Downloaded / { if (label != "Downloading dependencies") stage("Downloading dependencies"); next }
        /^Updating / { stage("Preparing dependencies"); next }
        /^Blocking / { stage("Waiting for Cargo lock"); next }
        /^Finished / || /^Installing / || /^Replacing / { stage("Installing Super CLI"); next }
        /^info: syncing channel/ { stage("Checking Rust release"); next }
        /^info: downloading/ { stage("Downloading Rust"); next }
        /^info: installing/ { stage("Installing Rust"); next }
        # Rustup can put concurrent component bars in one CR-delimited record.
        # Prefer an active component over another one waiting for installation.
        /(rustc|cargo|rust-std)[[:space:]]+(downloading|unpacking|installing|installed|pending)/ {
            text=$0
            gsub(/(rustc|cargo|rust-std)[[:space:]]+(downloading|unpacking|installing|installed|pending)/, "\n&", text)
            parts=split(text, components, "\n"); active_component=0
            for (part=1; part<=parts; part++) {
                split(components[part], fields, /[[:space:]]+/)
                component=fields[1]; action=fields[2]
                if (component !~ /^(rustc|cargo|rust-std)$/) continue
                if (action == "downloading" || action == "unpacking") {
                    active_component=1
                    stage((action == "downloading" ? "Downloading Rust" : "Unpacking Rust") ": " component)
                    if (!match(components[part], /\[[# ]+\]/)) continue
                    bar=substr(components[part], RSTART+1, RLENGTH-2)
                    total=length(bar); done=gsub(/#/, "#", bar)
                    # Rustup reports a coarse bar rather than exact bytes received.
                    metric=sprintf("%d%%", int(100*done/total+0.5))
                } else if (!active_component) {
                    stage("Installing Rust: " component)
                }
            }
        }
        function row(text) {
            frame=frame text "\033[K"
            if (++rows < 6) frame=frame "\n\r"
        }
        function step(number, title) {
            if (number < phase) return "  " pass "✓" reset " " title
            if (number > phase) return "  " muted "○ " title reset
            return "  " accent frames[tick%10+1] reset " " bold title reset
        }
        END {
            split("⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏", frames, " ")
            if (label == "Building Super CLI") label="Building"
            if (label == "Installing the Super CLI" || label == "Installing Super CLI") label="Installing"
            # Each log-derived phrase gets its own animation clock, including
            # transitions inside one Cargo command. Keep the initial frame plain.
            activity=mode "|" label "|" (total > 0)
            getline previous_activity < activity_file
            getline started < activity_file
            close(activity_file)
            if (activity != previous_activity || started !~ /^[0-9]+$/ || tick < started) {
                started=tick
                print activity "\n" started > activity_file
                close(activity_file)
            }
            dots=""
            if (total <= 0) {
                beat=int((tick-started)/6)
                if (beat > 0) dots=substr("...", 1, (beat-1)%3+1)
            }
            if (mode == "skill") {
                if (length(label) > width-10) label=substr(label, 1, width-10)
                printf "\r    %s%s%s %s%-3s\033[K", accent, frames[tick%10+1], reset, label, dots
                exit
            }
            if (length(label) > width-8) label=substr(label, 1, width-8)
            row(step(1, "Prepare"))
            row(step(2, "Install Super CLI"))
            row(skills == "true" ? step(3, "Set up your agent") : "  " muted "– Agent setup skipped" reset)
            row("")
            row("    " muted label sprintf("%-3s", dots) reset)
            bar=""
            if (total > 0) {
                cells=width-length(metric)-7
                if (cells > 20) cells=20
                if (cells < 1) cells=1
                filled=int(cells*done/total+0.5)
                for (i=0; i<cells; i++)
                    bar=bar (i < filled ? accent "━" : muted "─")
                bar="    " bar reset "  " metric
            }
            row(bar)
            # Repaint in one write without clearing the old frame first.
            printf "\033[5A\r%s", frame
        }' >&2
}

start_progress() {
    next_progress_mode=${2:-install}
    reuse_progress=false
    if [ "${progress_mode:-}" = install ] && [ "$next_progress_mode" = install ] &&
        [ -n "$progress_visible_file" ] && [ -f "$progress_visible_file" ]; then
        reuse_progress=true
    fi
    stop_progress "$reuse_progress"
    progress_label=$1
    progress_mode=$next_progress_mode
    : >"$install_log"
    if [ "$interactive" = true ]; then
        if [ "$reuse_progress" = false ]; then
            # Keep signals enabled, but prevent terminal echo from moving our
            # cursor. Nonblocking reads let the renderer discard incidental input.
            if tty_settings=$(stty -g 2>/dev/null </dev/tty); then
                if stty -echo -icanon min 0 time 0 2>/dev/null </dev/tty; then
                    progress_input=true
                else
                    restore_tty
                fi
            fi
        fi
        progress_file="${install_log}.progress"
        progress_visible_file="${install_log}.visible"
        progress_rows=6
        progress_delay=0
        if [ "$progress_mode" = skill ]; then
            progress_rows=1
            progress_delay=5
        fi
        : >"$progress_file"
        if [ "$progress_delay" -eq 0 ]; then
            if [ "$reuse_progress" = false ]; then show_progress; fi
            progress_frame 0
        fi
        (
            trap - 0
            trap 'exit 0' HUP INT TERM
            tick=1
            while [ -f "$progress_file" ]; do
                sleep 0.1
                [ -f "$progress_file" ] || exit 0
                if [ "$tick" -ge "$progress_delay" ]; then
                    [ -f "$progress_visible_file" ] || show_progress
                    progress_frame "$tick"
                fi
                tick=$((tick + 1))
            done
        ) &
        spinner_pid=$!
    else
        printf '  %s…\n' "$progress_label" >&2
    fi
}

restore_tty() {
    if [ -n "$tty_settings" ]; then
        stty "$tty_settings" 2>/dev/null </dev/tty || :
        tty_settings=''
    fi
}

cleanup() {
    stop_progress
    if [ -n "$tty_settings" ]; then printf '\n' >&2; fi
    restore_tty
    if [ -n "$skill_work" ]; then rm -rf "$skill_work"; fi
    if [ -n "$command_pid" ]; then
        kill "$command_pid" 2>/dev/null || :
        wait "$command_pid" 2>/dev/null || :
    fi
    if [ -n "$install_log" ]; then
        rm -f "$install_log" "${install_log}.activity"
    fi
}

die() {
    stop_progress
    if [ -s "$install_log" ]; then
        # Upstream progress contains cursor controls. Keep diagnostics readable
        # without replaying those controls into the restored terminal.
        LC_ALL=C tr '\r' '\n' <"$install_log" | LC_ALL=C awk '
            { gsub(/\033\[[0-9;?]*[ -\/]*[@-~]/, "") }
            /^[[:space:]]*(Building|Downloading)[[:space:]]+\[/ { next }
            NF { print }
        ' >&2
    fi
    printf '  %s!%s %s\n\n' "$red" "$reset" "$1" >&2
    exit 1
}

run_quiet() {
    CARGO_TERM_PROGRESS_WHEN=always CARGO_TERM_PROGRESS_WIDTH=100 \
        CARGO_TERM_PROGRESS_TERM_INTEGRATION=false CARGO_TERM_COLOR=never \
        CARGO_TERM_QUIET=false CARGO_TERM_HYPERLINKS=false \
        RUSTUP_TERM_PROGRESS_WHEN=always RUSTUP_TERM_WIDTH=100 RUSTUP_TERM_COLOR=never \
        "$@" >"$install_log" 2>&1 &
    command_pid=$!
    command_status=0
    wait "$command_pid" || command_status=$?
    command_pid=''
    return "$command_status"
}

trap cleanup 0
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

supported_rust() {
    case "$1" in
        'rustc '*) rust_version=${1#rustc } ;;
        *) return 1 ;;
    esac
    rust_version=${rust_version%% *}
    rust_major=${rust_version%%.*}
    rust_minor=${rust_version#*.}
    rust_minor=${rust_minor%%.*}
    case "$rust_major:$rust_minor" in
        *[!0-9:]*|:*|*:) return 1 ;;
    esac
    [ "$rust_major" -gt "$super_rust_major" ] ||
        { [ "$rust_major" -eq "$super_rust_major" ] && [ "$rust_minor" -ge "$super_rust_minor" ]; }
}

prepare_rustup() {
    if command -v rustup >/dev/null 2>&1; then
        super_rustup=$(command -v rustup)
    elif [ -x "$cargo_home/bin/rustup" ]; then
        super_rustup="$cargo_home/bin/rustup"
    else
        # A system or Homebrew Rust can coexist with this installer-only rustup.
        # Do not add its proxies to PATH or change shell startup files.
        command -v curl >/dev/null 2>&1 || die 'Please install curl and try again.'
        super_bootstrap="$cargo_home/super-installer/bootstrap"
        RUSTUP_HOME="$cargo_home/super-installer/rustup"
        export RUSTUP_HOME
        super_rustup="$super_bootstrap/bin/rustup"
        if [ ! -x "$super_rustup" ]; then
            start_progress 'Downloading Rust installer'
            rustup_script=$(curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs 2>"$install_log") ||
                die 'Could not download Rust.'
            start_progress 'Installing Rust'
            CARGO_HOME="$super_bootstrap" RUSTUP_INIT_SKIP_PATH_CHECK=yes \
                run_quiet sh -c "$rustup_script" -- -y --no-modify-path --default-toolchain none --profile minimal ||
                die 'Could not prepare Rust for installing the Super CLI.'
        fi
    fi

    start_progress 'Installing Rust'
    run_quiet "$super_rustup" toolchain install "$super_rust_toolchain" --profile minimal --no-self-update ||
        die 'Could not install the Rust version the Super CLI needs.'
    super_rustc=$("$super_rustup" which --toolchain "$super_rust_toolchain" rustc 2>"$install_log") ||
        die 'Could not find the Rust version the Super CLI needs.'
    rust_version=$("$super_rustc" --version 2>"$install_log") ||
        die 'Could not run the Rust version the Super CLI needs.'
    supported_rust "$rust_version" || die 'The installed Rust version is too old for the Super CLI.'
}

# Install only the published Schematic skill. No Node.js or agent CLI is needed.
skill_notice() { printf '    %s%s%s\n' "$muted" "$*" "$reset" >&2; }
skill_warning() { printf '    %s!%s %s\n' "$amber" "$reset" "$*" >&2; }

skill_prompt() {
    printf '  %s%s?%s %sAdd the Super skill to these agents?%s\n' "$purple" "$bold" "$reset" "$bold" "$reset" >&2
    # Wrap only between agent names; commands and URLs stay copyable.
    prompt_width=80
    if prompt_size=$(stty size 2>/dev/null </dev/tty); then
        prompt_columns=${prompt_size##* }
        case "$prompt_columns" in
            ''|*[!0-9]*) ;;
            *) if [ "$prompt_columns" -ge 40 ]; then prompt_width=$prompt_columns; fi ;;
        esac
    fi
    prompt_line=''
    for agent in $new_agents; do
        agent_settings "$agent"
        candidate="${prompt_line:+$prompt_line, }$agent_label"
        if [ -n "$prompt_line" ] && [ "${#candidate}" -gt "$((prompt_width - 6))" ]; then
            printf '    %s,\n' "$prompt_line" >&2
            prompt_line=$agent_label
        else
            prompt_line=$candidate
        fi
    done
    printf '    %s\n    %s[Y/n] ›%s ' "$prompt_line" "$muted" "$reset" >&2
}

read_skill_answer() {
    answer=n
    if tty_settings=$(stty -g 2>/dev/null </dev/tty) &&
        stty -icanon -echo min 1 time 0 2>/dev/null </dev/tty; then
        key_prompted=false
        while :; do
            # Read /dev/tty, not the piped installer script. Keep the reader as
            # a tracked child so interruption can restore the terminal promptly.
            dd if=/dev/tty of="$install_log" bs=1 count=1 2>/dev/null &
            command_pid=$!
            # Register the reader before exposing the prompt so an immediate
            # Ctrl+C can stop it, rather than leaving it attached to the terminal.
            if [ "$key_prompted" = false ]; then
                skill_prompt
                key_prompted=true
            fi
            key_status=0
            wait "$command_pid" || key_status=$?
            command_pid=''
            [ "$key_status" -eq 0 ] || break
            # The sentinel preserves Enter and distinguishes it from EOF.
            key=$(cat "$install_log"; printf '.')
            key=${key%.}
            case "$key" in
                [Yy]|'
'|"$(printf '\r')") answer=y; break ;;
                [Nn]|''|"$(printf '\004')"|"$(printf '\033')") break ;;
                "$(printf '\003')") exit 130 ;;
            esac
        done
        restore_tty
        printf '%s\n' "$answer" >&2
    else
        restore_tty
        skill_prompt
        IFS= read -r answer </dev/tty || answer=n
    fi
}

skill_hash() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d ' ' -f 1
    else
        shasum -a 256 "$1" | cut -d ' ' -f 1
    fi
}

skill_unchanged() (
    target=$1
    [ ! -L "$target" ] && [ -f "$target/.super-installer-manifest" ] || exit 1
    count=1
    while read -r expected path extra; do
        [ -z "$extra" ] && [ -f "$target/$path" ] && [ ! -L "$target/$path" ] || exit 1
        [ "$(skill_hash "$target/$path")" = "$expected" ] || exit 1
        count=$((count + 1))
    done <"$target/.super-installer-manifest"
    [ -z "$(find "$target" -type l -print)" ] || exit 1
    [ "$(find "$target" -type f | wc -l | tr -d ' ')" = "$count" ]
)

skill_download() {
    skill_work=$(mktemp -d "${TMPDIR:-/tmp}/super-skill.XXXXXXXX") || return 1
    skill_base="$skill_origin/.well-known/skills/schematic-supertests"
    curl -fsSL --connect-timeout 15 --max-time 60 "$skill_base/install.txt" -o "$skill_work/install.txt" || return 1
    IFS=' ' read -r marker skill_version extra <"$skill_work/install.txt" || return 1
    [ "$marker" = version ] && [ -z "$extra" ] && [ "${#skill_version}" -eq 64 ] || return 1
    case "$skill_version" in *[!a-f0-9]*) return 1 ;; esac
    mkdir "$skill_work/files" || return 1
    sed '1d' "$skill_work/install.txt" >"$skill_work/manifest" || return 1
    [ "$(skill_hash "$skill_work/manifest")" = "$skill_version" ] || return 1
    count=0
    while read -r expected path extra; do
        [ -z "$extra" ] && [ "${#expected}" -eq 64 ] || return 1
        case "$expected" in *[!a-f0-9]*) return 1 ;; esac
        # Disallow traversal, absolute paths, shell syntax, and hidden files.
        case "$path" in ''|/*|.*|*..*|*/.*|*//*|*[!a-zA-Z0-9_./-]*) return 1 ;; esac
        case "$path" in *.md) ;; *) return 1 ;; esac
        [ ! -e "$skill_work/files/$path" ] || return 1
        count=$((count + 1))
        [ "$count" -le 100 ] || return 1
        mkdir -p "$skill_work/files/$(dirname "$path")" || return 1
        curl -fsSL --connect-timeout 15 --max-time 60 "$skill_base/versions/$skill_version/$path" -o "$skill_work/files/$path" || return 1
        [ "$(skill_hash "$skill_work/files/$path")" = "$expected" ] || return 1
    done <"$skill_work/manifest"
    [ -s "$skill_work/files/SKILL.md" ] || return 1
    cp "$skill_work/manifest" "$skill_work/files/.super-installer-manifest" || return 1
}

cleanup_skill_target() {
    # Arguments: staging, backup, destination, lock, previous path, secondary lock.
    [ -z "$1" ] || rm -rf "$1"
    # Restore the previous skill if replacement failed or was interrupted.
    if [ -n "$2" ] && [ ! -e "$3" ] && [ ! -L "$3" ]; then
        mv "$2/skill" "$5" 2>/dev/null || :
    fi
    rmdir "$4" 2>/dev/null || :
    [ -z "$6" ] || rmdir "$6" 2>/dev/null || :
}

skill_present() { [ -e "$1" ] || [ -L "$1" ]; }

skill_install_target() (
    umask 077
    target=$1
    agent=$2
    legacy=$3
    secondary_legacy=$4
    previous=$target
    if ! skill_present "$target" && skill_present "$legacy"; then previous=$legacy; fi
    parent=$(dirname "$target")
    mkdir -p "$parent" || exit 1
    # Share the lock with older installers during the skill-name migration.
    lock="$parent/.super-supertests.install-lock"
    mkdir "$lock" 2>/dev/null || { skill_warning "Another skill installation is using $parent. Try again when it finishes."; exit 1; }
    stage=''
    backup=''
    secondary_lock=''
    trap 'cleanup_skill_target "$stage" "$backup" "$target" "$lock" "$previous" "$secondary_lock"' 0
    trap 'exit 1' HUP INT TERM
    stage=$(mktemp -d "$parent/.super-skill.XXXXXXXX") || exit 1
    cp -R "$skill_work/files/." "$stage/" || exit 1
    preserve=false
    action=added
    if skill_present "$previous"; then
        action=updated
        skill_unchanged "$previous" || preserve=true
        backup=$(mktemp -d "$parent/.schematic-supertests.backup.XXXXXXXX") || exit 1
        mv "$previous" "$backup/skill" || exit 1
    fi
    mv "$stage" "$target" || exit 1
    stage=''
    printf '    %s✓%s %-16s %sSkill %s%s\n' "$green" "$reset" "$agent" "$muted" "$action" "$reset" >&2
    if [ -n "$backup" ]; then
        if [ "$preserve" = true ]; then
            skill_notice 'Previous skill saved at:'
            printf '      %s\n' "$backup/skill" >&2
        else
            rm -rf "$backup"
        fi
        backup=''
    fi
    # If both names existed, retire the old copy only after the new one is ready.
    for legacy in "$legacy" "$secondary_legacy"; do
        skill_present "$legacy" || continue
        legacy_parent=$(dirname "$legacy")
        if [ "$legacy_parent" != "$parent" ]; then
            candidate_lock="$legacy_parent/.super-supertests.install-lock"
            mkdir "$candidate_lock" 2>/dev/null || {
                skill_warning "Another skill installation is using $legacy_parent. Try again when it finishes."
                exit 1
            }
            secondary_lock=$candidate_lock
        fi
        preserve=false
        skill_unchanged "$legacy" || preserve=true
        legacy_backup=$(mktemp -d "$legacy_parent/.schematic-supertests.backup.XXXXXXXX") || exit 1
        mv "$legacy" "$legacy_backup/skill" || exit 1
        if [ "$preserve" = true ]; then
            skill_notice 'Previous skill saved at:'
            printf '      %s\n' "$legacy_backup/skill" >&2
        else
            rm -rf "$legacy_backup"
        fi
        if [ -n "$secondary_lock" ]; then
            rmdir "$secondary_lock" || exit 1
            secondary_lock=''
        fi
    done
)

agent_settings() {
    agent_secondary_legacy=''
    case "$1" in
        codex)
            agent_config=${CODEX_HOME:-$HOME/.codex}
            agent_skills="$HOME/.agents/skills"
            if ! skill_present "$agent_skills/schematic-supertests" &&
                ! skill_present "$agent_skills/super-supertests" &&
                { skill_present "$agent_config/skills/schematic-supertests" ||
                    skill_present "$agent_config/skills/super-supertests"; }; then
                agent_skills="$agent_config/skills"
            fi
            if [ "$agent_skills" != "$agent_config/skills" ]; then
                agent_secondary_legacy="$agent_config/skills/super-supertests"
            fi
            agent_label=Codex; agent_commands=codex ;;
        claude)
            agent_config=${CLAUDE_CONFIG_DIR:-$HOME/.claude}
            agent_label='Claude Code'; agent_commands=claude ;;
        opencode)
            agent_config=${XDG_CONFIG_HOME:-$HOME/.config}/opencode
            agent_label=OpenCode; agent_commands=opencode ;;
        cursor)
            agent_config="$HOME/.cursor"
            agent_label=Cursor; agent_commands='cursor cursor-agent' ;;
        copilot)
            agent_config="$HOME/.copilot"
            agent_label='GitHub Copilot'; agent_commands=copilot ;;
        gemini)
            agent_config="$HOME/.gemini"
            agent_label='Gemini CLI'; agent_commands=gemini ;;
        windsurf)
            agent_config="$HOME/.codeium/windsurf"
            agent_label=Windsurf; agent_commands=windsurf ;;
    esac
    if [ "$1" != codex ]; then
        agent_skills="$agent_config/skills"
    fi
    agent_target="$agent_skills/schematic-supertests"
    agent_legacy="$agent_skills/super-supertests"
}

agent_detected() {
    [ -d "$agent_config" ] && return 0
    if [ "$1" = cursor ] && [ -d "$HOME/.local/share/cursor-agent" ]; then return 0; fi
    for agent_command in $agent_commands; do
        command -v "$agent_command" >/dev/null 2>&1 && return 0
    done
    if [ "$1" = copilot ]; then
        [ -d "${XDG_CONFIG_HOME:-$HOME/.config}/github-copilot" ] && return 0
        for extension in "$HOME"/.vscode/extensions/github.copilot-* "$HOME"/.vscode-insiders/extensions/github.copilot-*; do
            [ -d "$extension" ] && return 0
        done
    fi
    return 1
}

setup_agents() {
    [ "$install_skills" = true ] || return 0
    new_agents=''
    existing=''
    for agent in codex claude opencode cursor copilot gemini windsurf; do
        agent_settings "$agent"
        if skill_present "$agent_target" || skill_present "$agent_legacy"; then
            existing="$existing $agent"
        elif agent_detected "$agent"; then
            new_agents="$new_agents $agent"
        fi
    done
    if [ -z "$new_agents$existing" ]; then
        printf '  %s– Agent setup skipped. No supported agents detected.%s\n' "$muted" "$reset" >&2
        return 0
    fi
    printf '\n  %sSet up your agent%s\n' "$bold" "$reset" >&2
    selected=$existing
    if [ -n "$new_agents" ]; then
        answer=n
        if [ "$assume_yes" = true ]; then
            answer=y
        elif { true </dev/tty; } 2>/dev/null; then
            read_skill_answer
            printf '\n' >&2
        else
            skill_notice 'Agent setup needs confirmation.'
            skill_notice 'Rerun the installer in a terminal or with --yes to add new skills.'
        fi
        case "$answer" in ''|[Yy]|[Yy][Ee][Ss]) selected="$selected$new_agents" ;; esac
    fi
    if [ -z "$selected" ]; then
        if { true </dev/tty; } 2>/dev/null; then
            skill_notice 'Skipped. You can rerun the installer to add the skill later.'
        fi
        return 0
    fi
    if ! command -v sha256sum >/dev/null 2>&1 && ! command -v shasum >/dev/null 2>&1; then
        skill_warning 'Skill setup needs shasum or sha256sum.'
        skill_notice 'The CLI is ready. Install either tool, then rerun the installer.'
        return 0
    fi
    start_progress 'Downloading Super skill' skill
    if ! skill_download >"$install_log" 2>&1; then
        stop_progress
        skill_warning 'The skill could not be downloaded or verified.'
        if [ -s "$install_log" ]; then sed 's/^/      /' "$install_log" >&2; fi
        skill_notice 'The CLI is ready. Rerun the installer to retry skill setup.'
        return 0
    fi
    stop_progress
    installed=false
    for agent in $selected; do
        agent_settings "$agent"
        if skill_install_target "$agent_target" "$agent_label" "$agent_legacy" "$agent_secondary_legacy"; then installed=true
        else
            skill_warning "Could not add the skill for $agent_label."
            skill_notice 'The CLI is ready. Rerun the installer to retry skill setup.'
        fi
    done
    if [ "$installed" = true ]; then
        printf '\n' >&2
        skill_notice 'Start a new agent session to use the Super skill.'
    fi
}

doc_link() {
    printf '    %s%-18s%s ' "$muted" "$1" "$reset" >&2
    if [ "$interactive" = true ]; then printf '\033]8;;%s\007' "$2" >&2; fi
    printf '%s%s%s%s' "$cyan" "$underline" "$2" "$reset" >&2
    if [ "$interactive" = true ]; then printf '\033]8;;\007' >&2; fi
    printf '\n' >&2
}

main() {
    for option in "$@"; do
        case "$option" in
            --yes|-y) assume_yes=true ;;
            --no-skills) install_skills=false ;;
            --help|-h) printf 'Usage: sh cli.sh [--yes] [--no-skills]\n'; return ;;
            *) printf 'Unknown installer option: %s\n' "$option" >&2; exit 1 ;;
        esac
    done
    case "$skill_origin" in
        https://*|http://127.0.0.1:*|http://localhost:*) ;;
        *) printf 'Skill downloads require HTTPS (or localhost for development).\n' >&2; exit 1 ;;
    esac

    install_log=$(mktemp "${TMPDIR:-/tmp}/super-install.XXXXXXXX") ||
        die 'Could not create the installer log.'
    printf '\n' >&2
    start_progress 'Preparing environment'
    cargo_home=${CARGO_HOME:-"$HOME/.cargo"}
    cargo_env="$cargo_home/env"
    restart=false

    if ! command -v cargo >/dev/null 2>&1 && [ -r "$cargo_env" ]; then
        . "$cargo_env"
        restart=true
    fi

    if ! command -v cargo >/dev/null 2>&1; then
        command -v curl >/dev/null 2>&1 || die 'Please install curl and try again.'
        start_progress 'Downloading Rust installer'
        rustup_script=$(curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs 2>"$install_log") ||
            die 'Could not download Rust.'
        start_progress 'Installing Rust'
        run_quiet sh -c "$rustup_script" -- -y --default-toolchain "$super_rust_toolchain" --profile minimal ||
            die 'Could not install Rust.'
        . "$cargo_env"
        restart=true
    fi

    if rust_version=$(rustc --version 2>/dev/null) && supported_rust "$rust_version"; then
        install_phase=2
        start_progress 'Installing the Super CLI'
        run_quiet cargo install super-tool || die 'Could not install the Super CLI.'
    else
        prepare_rustup
        install_phase=2
        start_progress 'Installing the Super CLI'
        # A system rustc earlier on PATH would otherwise override rustup's choice.
        RUSTC="$super_rustc" run_quiet "$super_rustup" run "$super_rust_toolchain" cargo install super-tool ||
            die 'Could not install the Super CLI.'
    fi
    stop_progress
    printf '  %s✓%s Prepare\n' "$green" "$reset" >&2
    printf '  %s✓%s Super CLI installed\n' "$green" "$reset" >&2
    setup_agents
    printf '\n  %sNext%s\n' "$bold" "$reset" >&2
    if [ "$restart" = true ]; then
        printf '    Restart your terminal, then run:\n' >&2
    fi
    printf '    %s%ssuper login%s\n' "$purple" "$bold" "$reset" >&2
    printf '\n  %sDocs%s\n' "$bold" "$reset" >&2
    doc_link 'First steps' "$docs_origin/super/get-started/"
    doc_link 'Using your agent' "$docs_origin/super/ai/"
    printf '\n' >&2
}

main "$@"
