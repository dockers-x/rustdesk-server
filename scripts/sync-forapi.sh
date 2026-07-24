#!/usr/bin/env bash

set -euo pipefail

readonly origin_remote="${ORIGIN_REMOTE:-origin}"
readonly upstream_remote="${UPSTREAM_REMOTE:-upstream}"
readonly upstream_url="${UPSTREAM_URL:-https://github.com/rustdesk/rustdesk-server.git}"
readonly upstream_branch="${UPSTREAM_BRANCH:-master}"
readonly forapi_branch="${FORAPI_BRANCH:-forapi}"
readonly base_branch="${FORAPI_BASE_BRANCH:-forapi-base}"
readonly mode="${1:-sync}"

fail() {
    printf '错误：%s\n' "$*" >&2
    exit 1
}

resolve_ref() {
    git rev-parse --verify "$1^{commit}" 2>/dev/null
}

rebase_in_progress() {
    [[ -d "$(git rev-parse --git-path rebase-merge)" || -d "$(git rev-parse --git-path rebase-apply)" ]]
}

[[ "$mode" == "sync" || "$mode" == "--continue" ]] || fail "用法：$0 [--continue]"
[[ $# -le 1 ]] || fail "用法：$0 [--continue]"

repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || fail "请在 Git 仓库中运行此脚本"
cd "$repo_root"

if [[ "$mode" == "--continue" ]] && rebase_in_progress; then
    [[ -z "$(git diff --name-only --diff-filter=U)" ]] || fail "仍有未解决的冲突；解决后执行 git add，再重新运行 $0 --continue"

    if ! GIT_EDITOR=true git rebase --continue; then
        if rebase_in_progress; then
            printf '\n仍有冲突；解决后执行 git add，再重新运行 %s --continue。\n' "$0" >&2
        fi
        exit 1
    fi
fi

current_branch=$(git branch --show-current)
[[ "$current_branch" == "$forapi_branch" ]] || fail "请先切换到 $forapi_branch 分支"

[[ -z "$(git status --porcelain=v1 --untracked-files=all)" ]] || fail "工作区不干净，请先提交或处理本地修改"

git remote get-url "$origin_remote" >/dev/null 2>&1 || fail "找不到远端 $origin_remote"

if ! git remote get-url "$upstream_remote" >/dev/null 2>&1; then
    printf '添加上游远端 %s -> %s\n' "$upstream_remote" "$upstream_url"
    git remote add "$upstream_remote" "$upstream_url"
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
new_base=$(resolve_ref "$upstream_master_ref") || fail "上游缺少 $upstream_branch 分支"
local_forapi=$(resolve_ref "refs/heads/$forapi_branch") || fail "本地缺少 $forapi_branch 分支"

[[ "$old_master" == "$old_base" ]] || fail "$upstream_branch 与 $base_branch 基点不一致，请先检查分支状态"

if local_master=$(resolve_ref "refs/heads/$upstream_branch"); then
    [[ "$local_master" == "$old_master" ]] || fail "本地 $upstream_branch 含有未同步提交，拒绝覆盖"
fi

if local_base=$(resolve_ref "refs/heads/$base_branch"); then
    [[ "$local_base" == "$old_base" ]] || fail "本地 $base_branch 与远端不一致，拒绝覆盖"
fi

git merge-base --is-ancestor "$old_base" "$new_base" || fail "上游历史发生了非快进改写，需要人工确认"

if [[ "$mode" == "sync" ]]; then
    [[ "$local_forapi" == "$old_forapi" ]] || fail "本地 $forapi_branch 与远端不一致，请先处理本地或远端提交"
    git merge-base --is-ancestor "$old_base" "$local_forapi" || fail "$forapi_branch 不是基于 $base_branch 的 patch stack"

    if [[ "$old_base" != "$new_base" ]]; then
        printf '将 %s 上的 patch 重放到最新上游...\n' "$forapi_branch"
        if ! git rebase --onto "$new_base" "$old_base" "$forapi_branch"; then
            printf '\n解决冲突并执行 git add 后，运行 %s --continue；如需放弃，执行 git rebase --abort。\n' "$0" >&2
            exit 1
        fi
        local_forapi=$(resolve_ref "refs/heads/$forapi_branch")
    else
        printf '%s 已基于最新上游，无需重放 patch。\n' "$forapi_branch"
    fi
else
    [[ "$local_forapi" != "$old_forapi" ]] || fail "没有需要继续完成的 rebase"
    git merge-base --is-ancestor "$new_base" "$local_forapi" || fail "$forapi_branch 尚未完整重放到最新上游"

    old_patch_count=$(git rev-list --count "$old_base..$old_forapi")
    new_patch_count=$(git rev-list --count "$new_base..$local_forapi")
    [[ "$old_patch_count" == "$new_patch_count" ]] || fail "重放后的 patch 数量异常，请人工检查"
fi

git branch --force "$upstream_branch" "$new_base"
git branch --force "$base_branch" "$new_base"

printf '确认推送前远端没有并发变化...\n'
git fetch --prune "$origin_remote"
[[ "$(resolve_ref "$remote_forapi_ref")" == "$old_forapi" ]] || fail "远端 $forapi_branch 已变化，停止推送"
[[ "$(resolve_ref "$remote_base_ref")" == "$old_base" ]] || fail "远端 $base_branch 已变化，停止推送"
[[ "$(resolve_ref "$remote_master_ref")" == "$old_master" ]] || fail "远端 $upstream_branch 已变化，停止推送"

printf '原子推送 %s、%s 和 %s...\n' "$upstream_branch" "$base_branch" "$forapi_branch"
git push --atomic \
    --force-with-lease="refs/heads/$forapi_branch:$old_forapi" \
    "$origin_remote" \
    "refs/heads/$upstream_branch:refs/heads/$upstream_branch" \
    "refs/heads/$base_branch:refs/heads/$base_branch" \
    "refs/heads/$forapi_branch:refs/heads/$forapi_branch"

printf '\n同步完成。发布时在 GitHub 创建新 tag，并将 Target 选择为 %s。\n' "$forapi_branch"
