# cpu-mem-widget

一个正方形置顶小浮窗，用面积图（积分）显示本机 CPU 与内存占用，颜色随数值从冷蓝走到红。

```powershell
cargo build --release
.\target\release\cpu-mem-widget.exe
```

## 操作

| 动作 | 效果 |
|---|---|
| 按住卡片任意位置拖动 | 移动窗口 |
| 拖左下角的两道斜线 | 等比缩放（130–400，右上角不动） |
| 点右上角 × | 退出 |
| Esc | 退出（窗口有焦点时） |

位置和大小记在 `%APPDATA%\cpu-mem-widget\pos.txt`（物理像素，四元组 `x,y,w,h`）。首次运行停在主显示器工作区右上角；位置跑到屏幕外会自动拉回来。

## 显示

- 上下两段，各是最近 **60 秒**（每秒 1 个采样）的面积图，新数据靠右。
- 颜色即数值：0% 冷蓝 → 中间过渡 → 85% 金 → 100% 红。填充的每一行像素按它自己的高度取色，所以 90% 的柱顶是橙红、底部仍是蓝灰。色阶烘成一张 1×512 的纹理交给 GPU 双线性采样，放大也看不到 banding。
- 曲线用单调三次插值（极值处切线归零，不会过冲），数字用当前值对应的色阶提亮后显示。
- 尺寸变化时整块 UI 等比缩放：所有间距、字号、圆角都按 `宽/190` 缩放。
- 数值来源：`GetSystemTimes` 时间差算 CPU，`GlobalMemoryStatusEx` 的 `dwMemoryLoad` 取内存，与任务管理器同口径。

## 开销（本机 125% 缩放 / Win10 22H2 实测）

| 指标 | 实测 |
|---|---|
| 空闲 CPU | 0.26 % 单核（≈0.03 % 总量） |
| 工作集 | ≈84 MB（私有 ≈58 MB） |
| 重绘 | 1 次/秒，`request_repaint_after(1s)` |

内存大头是 OpenGL 上下文（glow/winit），不是逻辑开销。

## Windows 上踩过的坑

1. **透明**：winit 在 Windows 上不实现 `with_transparent`，只置 flag 会得到一个不透明矩形。逐像素 alpha 要靠 `DwmEnableBlurBehindWindow`；blur 区域给一个**空区域**（`CreateRectRgn(0,0,0,0)` + `DWM_BB_BLURREGION`），既保留 alpha 又不会给窗口描一圈玻璃边。
2. **顶上那条 1px 黑线**：winit 在窗口上留了 `WS_CAPTION/WS_BORDER` 风格，非客户区从顶部切走 1px，DWM 在那条上画自己的边框色。两步解决：把 style 改成纯 `WS_POPUP`，再挂一个 `WM_NCCALCSIZE` 钩子返回 0，让客户区铺满整个窗口。
3. **方形阴影**：顶层窗口带 DWM 投影，浅色背景上会看到卡片外面套着一个灰色方框。`DWMWA_NCRENDERING_POLICY = DWMNCRP_DISABLED` 关掉。
4. **Win11 圆角**：Win11 会给顶层窗口自己画圆角和 1px 边框，压在透明卡片上会露出壳子的角。`DWMWA_WINDOW_CORNER_PREFERENCE = DONOTROUND` + `DWMWA_BORDER_COLOR = DWMWA_COLOR_NONE`（Win10 上这两个调用直接返回错误，无副作用）。
5. **任务栏图标**：`WS_EX_APPWINDOW` 压过 `WS_EX_TOOLWINDOW`，两个位都要改；winit 应用 viewport 选项时会重写 ex-style，所以前几帧反复写。
6. **不要用 eframe 的窗口持久化**：它按点/像素往返换算，在 125% 缩放下每次重启都会漂移，几轮之后把窗口丢到屏幕外。改为自己用物理像素记 `pos.txt`。

## 可调项（`src/main.rs` 顶部）

| 常量 | 作用 |
|---|---|
| `BG` | 卡片底色与不透明度，alpha 246 = 96%。想更透往下调到 ~200，想完全不透调到 255 |
| `MIN_SIDE` / `MAX_SIDE` | 缩放上下限（pt） |
| `HIST` / `TICK` | 历史点数与采样周期，默认 60 × 1s |
| `RAMP` | 值→颜色色阶的 6 个控制点 |
| `CORNER` / `PAD` / `BAR_H` | 圆角、内边距、图表高度（按 190pt 设计尺寸） |

## 开机自启（可选）

`Win+R` → `shell:startup` → 把 exe 的快捷方式放进去。
