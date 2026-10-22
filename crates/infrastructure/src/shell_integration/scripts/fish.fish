# @author kongweiguang
# Fish init-command 在用户 config.fish 后执行，包装现有 fish_prompt 并监听实际 preexec 事件。

if not set -q __kerminal_shell_integration_active
    if functions --copy fish_prompt __kerminal_user_fish_prompt >/dev/null 2>&1
        set -g __kerminal_shell_integration_active 1
        set -g __kerminal_prompt_ready 0
        set -g __kerminal_command_active 0

        # 用户 prompt 可能切换目录；不捕获或重排其输出，在执行后报告最终 PWD，且只用 builtin。
        # @author kongweiguang
        function fish_prompt
            set -l __kerminal_command_status $status
            if test "$__kerminal_command_active" = 1
                printf '\e]133;D;%d\a' $__kerminal_command_status
                set -g __kerminal_command_active 0
            end
            printf '\e]133;A\a'
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
