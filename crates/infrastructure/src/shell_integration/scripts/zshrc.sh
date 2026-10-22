# @author kongweiguang
# 只加载用户原始 zshrc；钩子单独拼接以供本地 wrapper 和远端 bootstrap 复用。

if [[ -r "$ZDOTDIR/.zshrc" ]]; then
    source "$ZDOTDIR/.zshrc"
fi
__kerminal_user_zdotdir=${ZDOTDIR:-$HOME}

# 让首个登录 shell 后续读取用户 .zlogin/.zlogout，子 shell 不会再次加载临时 hook。
ZDOTDIR=$__kerminal_user_zdotdir
