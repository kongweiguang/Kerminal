# @author kongweiguang
# 仅通过临时 ZDOTDIR wrapper 链接用户文件，不改写家目录下的任何配置。

if [[ -r "$ZDOTDIR/.zshenv" ]]; then
    source "$ZDOTDIR/.zshenv"
fi
__kerminal_user_zdotdir=${ZDOTDIR:-$HOME}
ZDOTDIR=$__kerminal_hook_zdotdir
