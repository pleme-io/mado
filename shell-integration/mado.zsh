# Mado shell integration for Zsh
# Source this file in .zshrc: source /path/to/mado.zsh
#
# Provides:
# - OSC 133 semantic prompt markers (prompt/command/output boundaries)
# - OSC 7 current working directory reporting
# - Window title updates (user@host:cwd)

# Guard: only run in interactive shells inside mado
[[ -o interactive ]] || return
[[ -n "$MADO_SHELL_INTEGRATION" ]] && return
export MADO_SHELL_INTEGRATION=1

__mado_osc() {
    printf '\e]%s\e\\' "$1"
}

# OSC 7: Report current working directory
__mado_report_cwd() {
    __mado_osc "7;file://${HOST}${PWD}"
}

# OSC 2: Set window title
__mado_update_title() {
    __mado_osc "2;${USER}@${HOST}:${PWD}"
}

# precmd: runs before each prompt
__mado_precmd() {
    local exit_code=$?
    # OSC 133;D — command finished with exit code
    __mado_osc "133;D;${exit_code}"
    # Report CWD and update title
    __mado_report_cwd
    __mado_update_title
    # OSC 133;A — prompt start
    __mado_osc "133;A"
}

# preexec: runs after user enters command, before execution
__mado_preexec() {
    # OSC 133;C — output start
    __mado_osc "133;C"
}

# Install hooks via zsh hook arrays
autoload -Uz add-zsh-hook
add-zsh-hook precmd __mado_precmd
add-zsh-hook preexec __mado_preexec

# Mark the initial prompt
__mado_report_cwd
__mado_update_title
__mado_osc "133;A"
