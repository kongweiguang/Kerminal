# @author kongweiguang
# 登录 shell 的用户 profile 必须在临时 hook rc 之前按原顺序执行。

if [[ -r "$ZDOTDIR/.zprofile" ]]; then
    source "$ZDOTDIR/.zprofile"
fi
__kerminal_user_zdotdir=${ZDOTDIR:-$HOME}
ZDOTDIR=$__kerminal_hook_zdotdir
