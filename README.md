# pa120-lite

利民 PA120 数显的轻量替代。没有 TRCC 的界面、视频和 WebView。

后台运行，开机自启。屏幕四格：

- CPU 温度、CPU 占用
- GPU 温度、GPU 占用

颜色跟着各自的占用走：30% 以下青色，30% 起绿色，50% 起黄色，70% 起橙色，90% 起红色。托盘图标上可以看当前读数，右键退出。

## 运行

需要 Windows，以及已安装的 [PawnIO](https://github.com/namazso/PawnIO.Setup)（读 Intel CPU 封装温度）。GPU 温度走系统里的 AMD 驱动。

把 `pa120-lite.exe` 运行一次就会写到当前用户的开机启动项。再开一次不会重复占用设备。

```text
pa120-lite
pa120-lite --brightness 50
```

亮度范围 0 到 100，默认 80。

## 编译

```text
cargo build --release
```

产物在 `target\release\pa120-lite.exe`。

`third_party/IntelMSR.bin` 来自 [PawnIO.Modules](https://github.com/namazso/PawnIO.Modules)，LGPL-2.1-or-later。本程序其余部分是 MIT。
