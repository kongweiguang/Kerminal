# @author kongweiguang
# 此文件在原 .bashrc 之后读取，只包裹 prompt/命令边界，不接管用户命令本身。

if [[ ${__kerminal_shell_integration_active-0} != 1 ]]; then
    # 不覆盖用户 DEBUG trap、只读提示符或无序的关联数组；它们无法透明串接。
    # declare 的标志可能组合为 -ar/-arx，只检查标志部分而不匹配变量正文。
    [[ -z $(trap -p DEBUG) ]] || return 0
    __kerminal_prompt_declaration=$(declare -p PROMPT_COMMAND 2>/dev/null) || __kerminal_prompt_declaration=
    __kerminal_prompt_flags=${__kerminal_prompt_declaration#declare -}
    __kerminal_prompt_flags=${__kerminal_prompt_flags%% *}
    case $__kerminal_prompt_flags in
        *r*|*A*) return 0 ;;
    esac
    __kerminal_prompt_string_declaration=$(declare -p PS1 2>/dev/null) || __kerminal_prompt_string_declaration=
    __kerminal_prompt_string_flags=${__kerminal_prompt_string_declaration#declare -}
    __kerminal_prompt_string_flags=${__kerminal_prompt_string_flags%% *}
    case $__kerminal_prompt_string_flags in
        *r*) return 0 ;;
    esac

    __kerminal_shell_integration_active=1
    __kerminal_prompt_ready=0
    __kerminal_command_active=0
    __kerminal_command_status=0
    __kerminal_edit_nonce='__KERMINAL_EDIT_NONCE__'
    __kerminal_edit_pid=$BASHPID

    # 输出字段沿用百分号 UTF-8 字节编码，变量承接结果避免命令替换改变编辑器状态。
    # @author kongweiguang
    __kerminal_edit_encode() {
        local LC_ALL=C __ke_text=$1 __ke_char __ke_hex __ke_i
        __kerminal_edit_encoded=
        for ((__ke_i=0; __ke_i<${#__ke_text}; __ke_i++)); do
            __ke_char=${__ke_text:__ke_i:1}
            case $__ke_char in
                [a-zA-Z0-9/._~-]) __kerminal_edit_encoded+=$__ke_char ;;
                *) builtin printf -v __ke_hex '%%%02X' "'$__ke_char"; __kerminal_edit_encoded+=$__ke_hex ;;
            esac
        done
    }

    # 在 C locale 按 UTF-8 首字节计数，使相同的 4000 标量限制不依赖用户 locale。
    # @author kongweiguang
    __kerminal_edit_within_limit() {
        local LC_ALL=C __ke_scalar
        (( ${#1} <= 16000 )) || return 1
        __ke_scalar=${1//[$'\200'-$'\277']/}
        (( ${#__ke_scalar} <= 4000 ))
    }

    # READLINE_POINT 按当前 locale 表示字符位置；先截取前缀再转字节数，避免中文光标落入 UTF-8 中间。
    # @author kongweiguang
    __kerminal_edit_snapshot() {
        local __ke_status=$? __ke_line __ke_cwd __ke_version __ke_prefix __ke_cursor
        [[ $BASHPID == "$__kerminal_edit_pid" && ${__kerminal_prompt_ready-0} == 1 ]] || return "$__ke_status"
        if ! __kerminal_edit_within_limit "$READLINE_LINE"; then
            __kerminal_edit_encode "$BASH_VERSION"
            builtin printf '\e]6977;Kerminal;1;R;%s;bash;input_too_long;%s\a' "$__kerminal_edit_nonce" "$__kerminal_edit_encoded"
            return "$__ke_status"
        fi
        __ke_prefix=${READLINE_LINE:0:READLINE_POINT}
        local LC_ALL=C
        __ke_cursor=${#__ke_prefix}
        __kerminal_edit_encode "$READLINE_LINE"; __ke_line=$__kerminal_edit_encoded
        __kerminal_edit_encode "$PWD"; __ke_cwd=$__kerminal_edit_encoded
        __kerminal_edit_encode "$BASH_VERSION"; __ke_version=$__kerminal_edit_encoded
        builtin printf '\e]6977;Kerminal;1;S;%s;bash;byte;%s;%s;%s;%s;0\a' "$__kerminal_edit_nonce" "$__ke_cursor" "$__ke_version" "$__ke_cwd" "$__ke_line"
        return "$__ke_status"
    }

    # 先检查所有模式再安装，任何失败均不宣称可用；回滚仅移除本轮确认原本未绑定的键。
    # vi 插入模式的真实名称是 vi-insert，不能使用 Readline 不接受的 vi-insertion。
    # @author kongweiguang
    __kerminal_edit_install() {
        local __ke_map __ke_flag __ke_listing __ke_rollback
        local -a __ke_installed_maps=()
        for __ke_map in emacs-standard vi-insert vi-command; do
            for __ke_flag in -p -s -X; do
                __ke_listing=$(builtin bind -m "$__ke_map" "$__ke_flag" 2>/dev/null) || return 1
                case $__ke_listing in *'"\e[24;8~":'*) return 1 ;; esac
            done
        done
        for __ke_map in emacs-standard vi-insert vi-command; do
            if ! builtin bind -m "$__ke_map" -x '"\e[24;8~":__kerminal_edit_snapshot' 2>/dev/null; then
                for __ke_rollback in "${__ke_installed_maps[@]}"; do
                    builtin bind -m "$__ke_rollback" -r '\e[24;8~' 2>/dev/null || :
                done
                return 1
            fi
            __ke_installed_maps+=("$__ke_map")
        done
    }
    __kerminal_edit_installed=0
    if __kerminal_edit_install; then __kerminal_edit_installed=1; fi
    __kerminal_edit_encode "$BASH_VERSION"
    builtin printf '\e]6977;Kerminal;1;H;%s;bash;%s;%s\a' "$__kerminal_edit_nonce" "$__kerminal_edit_installed" "$__kerminal_edit_encoded"

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

    # A 只在真正开始一轮 prompt 时输出，不能放进会被 bind-x 重绘的 PS1，否则只读快照也会推进 epoch。
    # B 保留在 prompt 文本之后；接收端把同一 A 后重复的 B 当作重绘，Ctrl+C/空回车仍有新 A。
    # @author kongweiguang
    __kerminal_finish_prompt() {
        if [[ ${__kerminal_command_active-0} == 1 ]]; then
            builtin printf '\e]133;D;%s\a' "${__kerminal_command_status-1}"
            __kerminal_command_active=0
        fi
        __kerminal_report_directory
        # 兼容已带同一标准包装的 PS1，只挪走精确的开头 A，不改提示符正文或其它用户转义。
        PS1=${PS1-'\s-\v\$ '}
        PS1=${PS1#'\[\e]133;A\a\]'}
        case ${PS1-} in
            *'\[\e]133;B\a\]'*) ;;
            *) PS1=${PS1-'\s-\v\$ '}'\[\e]133;B\a\]' ;;
        esac
        builtin printf '\e]133;A\a'
        __kerminal_prompt_ready=1
    }

    __kerminal_debug() {
        case ${BASH_COMMAND-} in
            __kerminal_capture_status*|__kerminal_finish_prompt*|__kerminal_debug*|__kerminal_edit_*) return 0 ;;
        esac
        if [[ ${__kerminal_prompt_ready-0} == 1 ]]; then
            __kerminal_prompt_ready=0
            __kerminal_command_active=1
            builtin printf '\e]133;C\a'
        fi
        return 0
    }

    case $__kerminal_prompt_flags in
        *a*)
            PROMPT_COMMAND=(__kerminal_capture_status "${PROMPT_COMMAND[@]}" __kerminal_finish_prompt)
            trap '__kerminal_debug' DEBUG
            ;;
        *)
            __kerminal_user_prompt_command=${PROMPT_COMMAND-}
            PROMPT_COMMAND='__kerminal_capture_status; if [[ -n ${__kerminal_user_prompt_command-} ]]; then eval "$__kerminal_user_prompt_command"; fi; __kerminal_finish_prompt'
            trap '__kerminal_debug' DEBUG
            ;;
    esac
fi
