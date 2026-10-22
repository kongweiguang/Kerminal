# @author kongweiguang
# 此文件在原 .bashrc 之后读取，只包裹 prompt/命令边界，不接管用户命令本身。

if [[ ${__kerminal_shell_integration_active-0} != 1 ]]; then
    # 不覆盖用户 DEBUG trap 或只读提示符变量；这两种语义无法透明串接。
    [[ -z $(trap -p DEBUG) ]] || return 0
    __kerminal_prompt_declaration=$(declare -p PROMPT_COMMAND 2>/dev/null) || __kerminal_prompt_declaration=
    case $__kerminal_prompt_declaration in
        *'declare -r'*) return 0 ;;
    esac
    __kerminal_prompt_string_declaration=$(declare -p PS1 2>/dev/null) || __kerminal_prompt_string_declaration=
    case $__kerminal_prompt_string_declaration in
        *'declare -r'*) return 0 ;;
    esac

    __kerminal_shell_integration_active=1
    __kerminal_prompt_ready=0
    __kerminal_command_active=0
    __kerminal_command_status=0

    # 按 UTF8 原始字节编码 URI；只用 shell builtin，目录中的控制字节不会成为终端协议。
    # @author kongweiguang
    __kerminal_report_directory() {
        local LC_ALL=C
        local __kerminal_directory=$PWD __kerminal_uri= __kerminal_char __kerminal_encoded __kerminal_index
        for ((__kerminal_index=0; __kerminal_index<${#__kerminal_directory}; __kerminal_index++)); do
            __kerminal_char=${__kerminal_directory:__kerminal_index:1}
            case $__kerminal_char in
                [a-zA-Z0-9/._~-]) __kerminal_uri+=$__kerminal_char ;;
                *) builtin printf -v __kerminal_encoded '%%%02X' "'$__kerminal_char"; __kerminal_uri+=$__kerminal_encoded ;;
            esac
        done
        builtin printf '\e]7;file://%s\a' "$__kerminal_uri"
    }

    __kerminal_capture_status() {
        __kerminal_command_status=$?
        __kerminal_prompt_ready=0
    }

    # 在用户 PROMPT_COMMAND 改完目录后报告实际 PWD，同时保留原 prompt 和命令退出码。
    # @author kongweiguang
    __kerminal_finish_prompt() {
        if [[ ${__kerminal_command_active-0} == 1 ]]; then
            builtin printf '\e]133;D;%s\a' "${__kerminal_command_status-1}"
            __kerminal_command_active=0
        fi
        __kerminal_report_directory
        case ${PS1-} in
            *'\[\e]133;B\a\]'*) ;;
            *) PS1='\[\e]133;A\a\]'${PS1-'\s-\v\$ '}'\[\e]133;B\a\]' ;;
        esac
        __kerminal_prompt_ready=1
    }

    __kerminal_debug() {
        case ${BASH_COMMAND-} in
            __kerminal_capture_status*|__kerminal_finish_prompt*|__kerminal_debug*) return 0 ;;
        esac
        if [[ ${__kerminal_prompt_ready-0} == 1 ]]; then
            __kerminal_prompt_ready=0
            __kerminal_command_active=1
            builtin printf '\e]133;C\a'
        fi
        return 0
    }

    case $__kerminal_prompt_declaration in
        'declare -a'*)
            PROMPT_COMMAND=(__kerminal_capture_status "${PROMPT_COMMAND[@]}" __kerminal_finish_prompt)
            trap '__kerminal_debug' DEBUG
            ;;
        'declare -A'*)
            # Bash associative PROMPT_COMMAND 没有等价的执行顺序语义，保持原提示符并关闭受管能力。
            ;;
        *)
            __kerminal_user_prompt_command=${PROMPT_COMMAND-}
            PROMPT_COMMAND='__kerminal_capture_status; if [[ -n ${__kerminal_user_prompt_command-} ]]; then eval "$__kerminal_user_prompt_command"; fi; __kerminal_finish_prompt'
            trap '__kerminal_debug' DEBUG
            ;;
    esac
fi
