# @author kongweiguang
# 将 prompt 与 command 状态挂到原有钩子顺序中，不替换用户配置目录或命令。

if [[ -o interactive && ${__kerminal_shell_integration_active-0} != 1 ]]; then
    typeset -g __kerminal_shell_integration_active=1
    typeset -g __kerminal_prompt_ready=0
    typeset -g __kerminal_command_active=0

    # 强制按 UTF8 字节编码 URI，不用外部命令；特殊目录名不能注入 OSC 终止符。
    # @author kongweiguang
    __kerminal_report_directory() {
        local LC_ALL=C
        local __kerminal_directory=$PWD __kerminal_uri= __kerminal_char __kerminal_encoded
        local -i __kerminal_index
        for ((__kerminal_index=1; __kerminal_index<=${#__kerminal_directory}; __kerminal_index++)); do
            __kerminal_char=${__kerminal_directory[__kerminal_index]}
            case $__kerminal_char in
                [a-zA-Z0-9/._~-]) __kerminal_uri+=$__kerminal_char ;;
                *) builtin printf -v __kerminal_encoded '%%%02X' "'$__kerminal_char"; __kerminal_uri+=$__kerminal_encoded ;;
            esac
        done
        builtin printf '\e]7;file://%s\a' "$__kerminal_uri"
    }

    __kerminal_capture_prompt_status() {
        local command_status=$?
        if [[ ${__kerminal_command_active-0} == 1 ]]; then
            print -rn -- $'\e]133;D;'"$command_status"$'\a'
            __kerminal_command_active=0
        fi
        return 0
    }

    # 最后一个 precmd 钩子读取最终 PWD，不查询远端或覆盖用户提示符的目录逻辑。
    # @author kongweiguang
    __kerminal_finish_prompt() {
        __kerminal_report_directory
        case ${PS1-} in
            *'133;B'*) ;;
            *) PS1=$'%{\e]133;A\a%}'"${PS1-}"$'%{\e]133;B\a%}' ;;
        esac
        __kerminal_prompt_ready=1
    }

    __kerminal_start_command() {
        if [[ ${__kerminal_prompt_ready-0} == 1 ]]; then
            __kerminal_prompt_ready=0
            __kerminal_command_active=1
            print -rn -- $'\e]133;C\a'
        fi
        return 0
    }

    typeset -ga precmd_functions preexec_functions
    precmd_functions=(__kerminal_capture_prompt_status ${precmd_functions:#__kerminal_capture_prompt_status})
    precmd_functions+=(__kerminal_finish_prompt)
    preexec_functions=(__kerminal_start_command ${preexec_functions:#__kerminal_start_command})
fi
