AcgDev HTTP Downloader 0.1.0 (Windows x64)
============================================

Rust + GPUI 重写版，兼容 aria2 的推送下载接口。


系统要求
--------
Windows 10 1809 及以上（x64）。
依赖系统自带的 DirectX 11 / DirectComposition / DirectWrite 与 ICU（icu.dll，Win10 1703 起随系统提供），
无需安装任何运行库，不需要 .NET，单个 exe 直接运行。


快速开始
--------
1. 双击 AcgDev.exe —— 打开窗口，默认下载目录是 exe 所在目录。
2. 命令行参数（在 cmd / PowerShell 中运行）：

   AcgDev.exe -d D:\Downloads         指定默认下载目录
   AcgDev.exe --rpc-port 6800          aria2 RPC 端口（默认 6800，写 0 关闭）
   AcgDev.exe --rpc-secret mytoken     RPC 密钥
   AcgDev.exe -j 8                    同时下载任务数（默认 5）
   AcgDev.exe --headless              不开窗口，只跑 RPC 服务
   AcgDev.exe --help                  帮助

   注意：exe 是 GUI 子系统程序，从 cmd 启动时提示符不会等待它退出，
   日志仍会打印在当前窗口。要独占终端请用 `start /wait` 或 --headless。


用 AriaNg / Motrix / YAAW 接管
------------------------------
窗口打开时 RPC 服务已在监听，直接添加一个 aria2 下载器：

   地址   http://127.0.0.1:6800/jsonrpc
   密钥   （未设置 --rpc-secret 则留空）

支持的 RPC 方法：
   添加   aria2.addUri / aria2.remove / aria2.forceRemove / aria2.pause
          aria2.unpause / aria2.forcePause / aria2.pauseAll / aria2.unpauseAll
          aria2.removeDownloadResult / aria2.purgeDownloadResult
   查询   aria2.tellStatus / aria2.getFiles / aria2.getUris / aria2.getGlobalStat
          aria2.getGlobalOption / aria2.getOption / aria2.getVersion
          aria2.getSessionInfo / system.listMethods / system.getCapabilities
          system.multicall
   变更   aria2.changeOption / aria2.changeGlobalOption / aria2.changePosition
   通知   WebSocket（ws://127.0.0.1:6800/jsonrpc）推送
          aria2.onDownloadStart / onDownloadPause / onDownloadStop /
          onDownloadComplete / onDownloadError

curl 直接推任务：

   curl -X POST http://127.0.0.1:6800/jsonrpc ^
     -H "Content-Type: application/json" ^
     -d "{\"jsonrpc\":\"2.0\",\"id\":\"1\",\"method\":\"aria2.addUri\",\"params\":[[\"https://example.com/file.iso\"]]}"


无窗口模式（做下载机/挂后台）
-----------------------------
   AcgDev.exe --headless -d D:\Downloads --rpc-port 6800 --rpc-secret mytoken

适合丢进「启动」目录或用任务计划程序自启，然后用 AriaNg 网页端管理。
按 Ctrl-C 退出。


行为说明
--------
- 断点续传：按 HTTP Range 分块下载，完成后校验长度；成功才把 .part 改名为目标文件。
- 写入：预分配 + 定位写（pwrite / WriteFile+seek），短写即报错，不会出现"进度 100% 但文件缺块"。
- 静默失败被禁止：任何写盘/校验错误都会让任务进入 error 状态并带上原因，不再伪造完成。


文件校验
--------
SHA256  AcgDev-0.1.0-win64.exe
        3a57c82f7c50d7c7658a251b2900d6ff7d7c78b261a15366f868ae8d0ae32935
大小    19075584 字节 (18.2 MiB)


排障
----
Q: 双击弹出「无法定位程序输入点 TaskDialogIndirect」/ 「找不到 icu.dll」？
A: 前者是 comctl32 v6 未激活，本版本已通过内嵌 manifest 修复；若仍出现，
   说明 exe 被改过或被某些"兼容性模式"接管，右键属性 → 兼容性 → 取消所有勾选。
   后者说明系统 ICU 被精简（Win10 1703 起 icu.dll 是系统组件，LTSC/精简镜像
   可能移除），换用完整系统或告知我改静态实现。

Q: 从 cmd 启动后提示符立刻返回？
A: exe 是 GUI 子系统，父 shell 不等待。要独占终端请用 `start /wait AcgDev.exe`，
   或直接用 `--headless`（它会阻塞）。


许可
----
MIT。图标字体等第三方组件许可见项目仓库。
