# Mado shell integration for Fish
# Source this file in config.fish: source /path/to/mado.fish
#
# Provides:
# - OSC 133 semantic prompt markers (prompt/command/output boundaries)
# - OSC 7 current working directory reporting
# - Window title updates (user@host:cwd)

# Guard: only install once
if set -q MADO_SHELL_INTEGRATION
    return
end
set -gx MADO_SHELL_INTEGRATION 1

function __mado_osc
    printf '\e]%s\e\\' $argv[1]
end

# OSC 7: Report current working directory
function __mado_report_cwd
    __mado_osc "7;file://"(hostname)"$PWD"
end

# OSC 2: Set window title
function __mado_update_title
    __mado_osc "2;$USER@"(hostname)":$PWD"
end

# fish_prompt wrapper: emit OSC 133;A before the prompt
function __mado_fish_prompt --on-event fish_prompt
    set -l last_status $status
    # OSC 133;D — command finished with exit code
    __mado_osc "133;D;$last_status"
    # Report CWD and update title
    __mado_report_cwd
    __mado_update_title
    # OSC 133;A — prompt start
    __mado_osc "133;A"
end

# fish_preexec: runs after user enters command, before execution
function __mado_fish_preexec --on-event fish_preexec
    # OSC 133;C — output start
    __mado_osc "133;C"
end

# Initial CWD report
__mado_report_cwd
__mado_update_title
