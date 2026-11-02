# @author kongweiguang
# Fish init-command 在用户 config.fish 后执行，包装现有 fish_prompt 并监听实际 preexec 事件。

if not set -q __kerminal_shell_integration_active
    if functions --copy fish_prompt __kerminal_user_fish_prompt >/dev/null 2>&1
        set -g __kerminal_shell_integration_active 1
        set -g __kerminal_prompt_ready 0
        set -g __kerminal_command_active 0
        set -g __kerminal_edit_nonce '__KERMINAL_EDIT_NONCE__'

        # commandline 只在绑定回调内读取；URL 编码防止行文本进入控制协议或普通输出。
        # @author kongweiguang
        function __kerminal_edit_snapshot
            test "$__kerminal_prompt_ready" = 1; or return
            set -l __ke_line (commandline --current-buffer | string collect)
            if test (string length -- "$__ke_line") -gt 4000
                set -l __ke_version (string escape --style=url -- "$version")
                printf '\e]6977;Kerminal;1;R;%s;fish;input_too_long;%s\a' "$__kerminal_edit_nonce" "$__ke_version"
                commandline --function repaint
                return
            end
            set -l __ke_cursor (commandline --cursor)
            set -l __ke_prediction '?'
            commandline --showing-suggestion >/dev/null 2>&1
            set -l __ke_prediction_status $status
            if test $__ke_prediction_status = 0
                set __ke_prediction 1
            else if test $__ke_prediction_status = 1
                set __ke_prediction 0
            end
            set -l __ke_input (string escape --style=url -- "$__ke_line")
            set -l __ke_cwd (string escape --style=url -- "$PWD")
            set -l __ke_version (string escape --style=url -- "$version")
            printf '\e]6977;Kerminal;1;S;%s;fish;scalar;%s;%s;%s;%s;%s\a' "$__kerminal_edit_nonce" "$__ke_cursor" "$__ke_version" "$__ke_cwd" "$__ke_input" "$__ke_prediction"
            commandline --function repaint
        end

        # 默认及 vi 插入模式有任一用户/预设绑定冲突时保留原绑定并声明不可用。
        set -g __kerminal_edit_installed 1
        for __ke_mode in default insert
            if bind --user --mode $__ke_mode \e\[24\;8\~ >/dev/null 2>&1; or bind --preset --mode $__ke_mode \e\[24\;8\~ >/dev/null 2>&1
                set -g __kerminal_edit_installed 0
            end
        end
        if test $__kerminal_edit_installed = 1
            for __ke_mode in default insert
                bind --user --mode $__ke_mode \e\[24\;8\~ __kerminal_edit_snapshot
            end
        end
        printf '\e]6977;Kerminal;1;H;%s;fish;%s;%s\a' "$__kerminal_edit_nonce" "$__kerminal_edit_installed" (string escape --style=url -- "$version")

        # fish_prompt 事件只在新一轮提示符前触发，不因 commandline repaint 重复；先完成命令再发布 A。
        # @author kongweiguang
        function __kerminal_begin_prompt --on-event fish_prompt
            set -l __kerminal_command_status $status
            if test "$__kerminal_command_active" = 1
                printf '\e]133;D;%d\a' $__kerminal_command_status
                set -g __kerminal_command_active 0
            end
            printf '\e]133;A\a'
        end

        # Ctrl+C 取消未提交的输入会触发 fish_cancel 而不是 fish_prompt，仍须开始新 epoch。
        # @author kongweiguang
        function __kerminal_cancel_prompt --on-event fish_cancel
            printf '\e]133;A\a'
        end

        # 用户 prompt 可能切换目录；重绘只输出 B，在原内容执行后报告最终 PWD，且只用 builtin。
        # @author kongweiguang
        function fish_prompt
            __kerminal_user_fish_prompt
            # string 是 Fish builtin；编码后只还原路径分隔符，避免每次 prompt 启动外部进程。
            # @author kongweiguang
            set -l __kerminal_directory (string escape --style=url -- $PWD)
            set __kerminal_directory (string replace -a '%2F' '/' -- $__kerminal_directory)
            printf '\e]7;file://%s\a' $__kerminal_directory
            printf '\e]133;B\a'
            set -g __kerminal_prompt_ready 1
        end

        function __kerminal_start_command --on-event fish_preexec
            if test "$__kerminal_prompt_ready" = 1
                set -g __kerminal_prompt_ready 0
                set -g __kerminal_command_active 1
                printf '\e]133;C\a'
            end
        end
    end
end
