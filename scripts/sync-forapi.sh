#!/usr/bin/env bash

set -euo pipefail

readonly origin_remote="${ORIGIN_REMOTE:-origin}"
readonly upstream_remote="${UPSTREAM_REMOTE:-upstream}"
readonly upstream_url="${UPSTREAM_URL:-https://github.com/rustdesk/rustdesk-server.git}"
readonly upstream_branch="${UPSTREAM_BRANCH:-master}"
readonly forapi_branch="${FORAPI_BRANCH:-forapi}"
readonly base_branch="${FORAPI_BASE_BRANCH:-forapi-base}"

fail() {
    printf '错误：%s\n' "$*" >&2
    exit 1
}

resolve_ref() {
    git rev-parse --verify "$1^{commit}" 2>/dev/null
}

[[ $# -eq 0 ]] || fail "用法：$0"

repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || fail "请在 Git 仓库中运行此脚本"
cd "$repo_root"

current_branch=$(git branch --show-current)
[[ "$current_branch" == "$forapi_branch" ]] || fail "请先切换到 $forapi_branch 分支"

[[ -z "$(git status --porcelain=v1 --untracked-files=all)" ]] || fail "工作区不干净，请先提交或处理本地修改"

git remote get-url "$origin_remote" >/dev/null 2>&1 || fail "找不到远端 $origin_remote"

if ! git remote get-url "$upstream_remote" >/dev/null 2>&1; then
    printf '添加上游远端 %s -> %s\n' "$upstream_remote" "$upstream_url"
    git remote add "$upstream_remote" "$upstream_url"
elif [[ "$(git remote get-url "$upstream_remote")" != "$upstream_url" ]]; then
    printf '将上游远端 %s 重设为 RustDesk 官方仓库 %s\n' "$upstream_remote" "$upstream_url"
    git remote set-url "$upstream_remote" "$upstream_url"
fi

printf '获取 fork 与上游最新状态...\n'
git fetch --prune "$origin_remote"
git fetch --prune "$upstream_remote" "$upstream_branch"

remote_forapi_ref="refs/remotes/$origin_remote/$forapi_branch"
remote_base_ref="refs/remotes/$origin_remote/$base_branch"
remote_master_ref="refs/remotes/$origin_remote/$upstream_branch"
upstream_master_ref="refs/remotes/$upstream_remote/$upstream_branch"

old_forapi=$(resolve_ref "$remote_forapi_ref") || fail "远端缺少 $forapi_branch 分支"
old_base=$(resolve_ref "$remote_base_ref") || fail "远端缺少 $base_branch 分支"
old_master=$(resolve_ref "$remote_master_ref") || fail "远端缺少 $upstream_branch 分支"
new_master=$(resolve_ref "$upstream_master_ref") || fail "上游缺少 $upstream_branch 分支"
local_forapi=$(resolve_ref "refs/heads/$forapi_branch") || fail "本地缺少 $forapi_branch 分支"

if local_master=$(resolve_ref "refs/heads/$upstream_branch"); then
    [[ "$local_master" == "$old_master" ]] || fail "本地 $upstream_branch 含有未同步提交，拒绝覆盖"
fi
if local_base=$(resolve_ref "refs/heads/$base_branch"); then
    [[ "$local_base" == "$old_base" ]] || fail "本地 $base_branch 与远端不一致，拒绝覆盖"
fi

git merge-base --is-ancestor "$old_master" "$new_master" || fail "RustDesk 官方上游历史发生了非快进改写，需要人工确认"
git merge-base --is-ancestor "$old_base" "$new_master" || fail "$base_branch 与 RustDesk 官方上游历史不一致，需要人工确认"
[[ "$local_forapi" == "$old_forapi" ]] || fail "本地 $forapi_branch 与远端不一致，请先处理本地或远端提交"

if git merge-base --is-ancestor "$new_master" "$local_forapi"; then
    printf '%s 已包含最新 RustDesk 官方上游，无需合并。\n' "$forapi_branch"
else
    printf '将 RustDesk 官方 %s 合并到 %s...\n' "$upstream_branch" "$forapi_branch"
    if ! git merge --no-edit "$new_master"; then
        printf '\n官方上游合并存在冲突；请在本地 %s 分支解决并提交后推送。\n' "$forapi_branch" >&2
        exit 1
    fi
fi

git branch --force "$upstream_branch" "$new_master"
git branch --force "$base_branch" "$new_master"

printf '确认推送前远端没有并发变化...\n'
git fetch --prune "$origin_remote"
[[ "$(resolve_ref "$remote_forapi_ref")" == "$old_forapi" ]] || fail "远端 $forapi_branch 已变化，停止推送"
[[ "$(resolve_ref "$remote_base_ref")" == "$old_base" ]] || fail "远端 $base_branch 已变化，停止推送"
[[ "$(resolve_ref "$remote_master_ref")" == "$old_master" ]] || fail "远端 $upstream_branch 已变化，停止推送"

printf '原子推送 %s、%s 和 %s...\n' "$upstream_branch" "$base_branch" "$forapi_branch"
git push --atomic \
    --force-with-lease="refs/heads/$upstream_branch:$old_master" \
    --force-with-lease="refs/heads/$base_branch:$old_base" \
    --force-with-lease="refs/heads/$forapi_branch:$old_forapi" \
    "$origin_remote" \
    "refs/heads/$upstream_branch:refs/heads/$upstream_branch" \
    "refs/heads/$base_branch:refs/heads/$base_branch" \
    "refs/heads/$forapi_branch:refs/heads/$forapi_branch"

printf '\n同步完成。发布时在 GitHub 创建新 tag，并将 Target 选择为 %s。\n' "$forapi_branch"
