# @author kongweiguang
# 将 prompt 与 command 状态挂到原有钩子顺序中，不替换用户配置目录或命令。

if [[ -o interactive && ${__kerminal_shell_integration_active-0} != 1 ]]; then
    typeset -g __kerminal_shell_integration_active=1
    typeset -g __kerminal_prompt_ready=0
    typeset -g __kerminal_command_active=0
    typeset -g __kerminal_edit_nonce='__KERMINAL_EDIT_NONCE__'

    # 固定本函数选项后按字节编码，避免用户 KSH_ARRAYS 等选项改变协议索引。
    # @author kongweiguang
    __kerminal_edit_encode() {
        emulate -L zsh
        local LC_ALL=C __ke_text=$1 __ke_char __ke_hex
        local -i __ke_i
        __kerminal_edit_encoded=
        for ((__ke_i=1; __ke_i<=${#__ke_text}; __ke_i++)); do
            __ke_char=${__ke_text[__ke_i]}
            case $__ke_char in
                [a-zA-Z0-9/._~-]) __kerminal_edit_encoded+=$__ke_char ;;
                *) builtin printf -v __ke_hex '%%%02X' "'$__ke_char"; __kerminal_edit_encoded+=$__ke_hex ;;
            esac
        done
    }

    # 在 C locale 按 UTF-8 首字节计数，保持 Shell 选项与用户 locale 不影响协议限制。
    # @author kongweiguang
    __kerminal_edit_within_limit() {
        emulate -L zsh
        local LC_ALL=C __ke_scalar
        (( ${#1} <= 16000 )) || return 1
        __ke_scalar=${1//[$'\200'-$'\277']/}
        (( ${#__ke_scalar} <= 4000 ))
    }

    # ZLE 保留 BUFFER/CURSOR；启用 MULTIBYTE 时 CURSOR 为标量，否则由协议层校验字节边界。
    # @author kongweiguang
    __kerminal_edit_snapshot() {
        local __ke_unit=byte __ke_line __ke_cwd __ke_version
        [[ ${__kerminal_prompt_ready-0} == 1 ]] || return 0
        if ! __kerminal_edit_within_limit "$BUFFER"; then
            __kerminal_edit_encode "$ZSH_VERSION"
            builtin printf '\e]6977;Kerminal;1;R;%s;zsh;input_too_long;%s\a' "$__kerminal_edit_nonce" "$__kerminal_edit_encoded"
            zle redisplay
            return 0
        fi
        [[ -o multibyte ]] && __ke_unit=scalar
        __kerminal_edit_encode "$BUFFER"; __ke_line=$__kerminal_edit_encoded
        __kerminal_edit_encode "$PWD"; __ke_cwd=$__kerminal_edit_encoded
        __kerminal_edit_encode "$ZSH_VERSION"; __ke_version=$__kerminal_edit_encoded
        builtin printf '\e]6977;Kerminal;1;S;%s;zsh;%s;%s;%s;%s;%s;0\a' "$__kerminal_edit_nonce" "$__ke_unit" "$CURSOR" "$__ke_version" "$__ke_cwd" "$__ke_line"
        zle redisplay
    }

    # 不接管用户现有键位；所有常用 keymap 均为空才能公布增强读取能力。
    typeset -g __kerminal_edit_installed=1
    for __ke_map in emacs viins vicmd; do
        __ke_binding=$(bindkey -M "$__ke_map" $'\e[24;8~')
        [[ $__ke_binding == *' undefined-key' ]] || __kerminal_edit_installed=0
    done
    if [[ $__kerminal_edit_installed == 1 ]]; then
        zle -N __kerminal_edit_snapshot
        for __ke_map in emacs viins vicmd; do
            bindkey -M "$__ke_map" $'\e[24;8~' __kerminal_edit_snapshot
        done
    fi
    __kerminal_edit_encode "$ZSH_VERSION"
    builtin printf '\e]6977;Kerminal;1;H;%s;zsh;%s;%s\a' "$__kerminal_edit_nonce" "$__kerminal_edit_installed" "$__kerminal_edit_encoded"

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

    # A 仅随真正的新 prompt 输出，避免 ZLE 重绘 PS1 时把同一输入误认成新 epoch。
    # 兼容本集成旧版精确的 PS1 前缀；B 保留在提示符末尾，目录使用所有用户 precmd 之后的 PWD。
    # @author kongweiguang
    __kerminal_finish_prompt() {
        __kerminal_report_directory
        PS1=${PS1-}
        PS1=${PS1#$'%{\e]133;A\a%}'}
        case ${PS1-} in
            *'133;B'*) ;;
            *) PS1="${PS1-}"$'%{\e]133;B\a%}' ;;
        esac
        builtin printf '\e]133;A\a'
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
