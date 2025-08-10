#!/bin/bash

# CI监控脚本 - feat/http-mvp分支
# 每30秒检查一次CI状态并记录

REPO="hu00yan/rust-miniss"
BRANCH="feat/http-mvp"
LOG_FILE="ci_logs/ci_monitor.log"

echo "开始监控 ${REPO} 分支 ${BRANCH} 的CI状态..." | tee -a "$LOG_FILE"
echo "时间: $(date)" | tee -a "$LOG_FILE"
echo "----------------------------------------" | tee -a "$LOG_FILE"

while true; do
    echo "=== $(date) ===" | tee -a "$LOG_FILE"
    
    # 获取最新的workflow runs
    gh run list --repo "$REPO" --branch "$BRANCH" --limit 5 | tee -a "$LOG_FILE"
    
    echo "" | tee -a "$LOG_FILE"
    
    # 每30秒检查一次
    sleep 30
done
