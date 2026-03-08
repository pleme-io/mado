# Mado shell integration for Bash
# Source this file in .bashrc: source /path/to/mado.bash
#
# Provides:
# - OSC 133 semantic prompt markers (prompt/command/output boundaries)
# - OSC 7 current working directory reporting
# - Window title updates (user@host:cwd)

# Guard: only run in interactive shells inside mado
[[ $- == *i* ]] || return
[[ -n "$MADO_SHELL_INTEGRATION" ]] && return
export MADO_SHELL_INTEGRATION=1

__mado_osc() {
    printf '\033]%s\033\\' "$1"
}

# OSC 7: Report current working directory
__mado_report_cwd() {
    __mado_osc "7;file://$(hostname)$(pwd)"
}

# OSC 2: Set window title
__mado_update_title() {
    __mado_osc "2;${USER}@$(hostname):$(pwd)"
}

# OSC 133: Prompt start (marker A)
__mado_prompt_start() {
    __mado_osc "133;A"
}

# OSC 133: Command start (marker B) — after user presses Enter
__mado_command_start() {
    __mado_osc "133;B"
}

# OSC 133: Command finished (marker D) with exit code
__mado_command_end() {
    __mado_osc "133;D;$?"
}

# OSC 133: Output start (marker C)
__mado_output_start() {
    __mado_osc "133;C"
}

# Install hooks
if [[ -z "$__mado_installed" ]]; then
    __mado_installed=1

    # Prepend prompt markers to PS1
    PS1="\[$(__mado_prompt_start)\]${PS1}\[$(__mado_command_start)\]"

    # Use PROMPT_COMMAND for CWD reporting and title updates
    __mado_precmd() {
        __mado_command_end
        __mado_report_cwd
        __mado_update_title
    }

    if [[ -n "$PROMPT_COMMAND" ]]; then
        PROMPT_COMMAND="__mado_precmd;${PROMPT_COMMAND}"
    else
        PROMPT_COMMAND="__mado_precmd"
    fi

    # DEBUG trap for output start marker (runs before each command)
    __mado_preexec() {
        __mado_output_start
    }
    trap '__mado_preexec' DEBUG
fi
