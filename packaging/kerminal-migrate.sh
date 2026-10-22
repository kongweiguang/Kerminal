#!/bin/sh
# @author kongweiguang
set -eu

# 设计约束：只解析迁移模式与根目录；转换规则全部由同目录 Rust helper 负责。
# @author kongweiguang
print_usage() {
    printf '%s\n' 'Usage: kerminal-migrate.sh [--root PATH] (--dry-run | --apply)' >&2
}

root=
root_was_set=0
mode=

while [ "$#" -gt 0 ]; do
    case "$1" in
        --root)
            if [ "$root_was_set" -ne 0 ] || [ "$#" -lt 2 ] || [ -z "$2" ]; then
                print_usage
                exit 2
            fi
            root=$2
            root_was_set=1
            shift 2
            ;;
        --dry-run|--apply)
            if [ -n "$mode" ]; then
                print_usage
                exit 2
            fi
            mode=$1
            shift
            ;;
        --help)
            print_usage
            exit 0
            ;;
        *)
            print_usage
            exit 2
            ;;
    esac
done

if [ -z "$mode" ]; then
    print_usage
    exit 2
fi

if [ "$root_was_set" -eq 0 ]; then
    if [ -z "${HOME-}" ]; then
        printf '%s\n' 'The current user home directory is unavailable.' >&2
        exit 2
    fi
    root=$HOME/.kerminal
fi

script_path=$0
case "$script_path" in
    /*) ;;
    *) script_path=$PWD/$script_path ;;
esac
script_dir=$(CDPATH= cd -P "$(dirname "$script_path")" && pwd)
helper_path=$script_dir/kerminal-migrate

if [ -L "$helper_path" ] || [ ! -f "$helper_path" ] || [ ! -x "$helper_path" ]; then
    printf 'Migration helper is missing or not executable beside this script: %s\n' "$helper_path" >&2
    exit 127
fi

exec "$helper_path" --root "$root" "$mode"
