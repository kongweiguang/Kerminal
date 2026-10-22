# ConPTY 历史重画夹具

<!-- @author kongweiguang -->

来源：Windows 11 build 26200、portable-pty 本地 ConPTY（创建 flag 0x2）、隔离无配置 pwsh 7.6.5。固定输出 160 行 `R40-IDLE-NNN 中文 é` 后保持空闲，网格从 52×15 扩为 64×17，再缩回 52×15。内容均由诊断程序生成，不含用户终端数据。

三个 `.vt` 文件分别保存初始输出、扩容后重画、缩回后重画，仅拼接同阶段的原始输出块，不包含向 PTY 回写的 6 字节协议响应。回归须先更新引擎尺寸，再消费对应重画文件。扩容流从第 147 行开始重画，未重发 145、146；通用扩行拉入的这两行因此会被覆盖。ConPTY 策略保留旧坐标和历史后可保住全部 160 行。

原始 trace：`r40-idle-resize-45596-1792542891043.r40trace`，SHA-256 `729203e59c21744da8150fd49bbf874a4c6aa9643e43f2ec6d9fe84a5df0935c`。

| 文件 | 字节数 | SHA-256 |
|---|---:|---|
| conpty-history-initial.vt | 4152 | 3a4a40a168a3bdb125b1f35fef63a262edb3eefda82f7288e350fb659a329304 |
| conpty-history-expanded.vt | 454 | 406999ac18507c8c7e05810e0d4f9998f5a61cc43aca3ab7c42e2f149a0c818d |
| conpty-history-returned.vt | 430 | 2a7a4d9e967237c48250e886ee0040401060058ba2b27122933d72d85b5a5bc5 |

该测试固定的是 resize 与随后重画的兼容性，不是物理键盘、GUI 或长时稳定性验收。
