# Tauri + WebKitGTK：页面切换"闪一下旧页面"的成因与修法

> 本文来自云语聚合网盘（Yunyu Drive，Tauri 2 + Vue 3）的实际排查。同一个现象在其它 Tauri 桌面项目里
> 也很常见，且**只在打包成 deb 后出现、浏览器里完全正常**，所以可以直接套用。

## 症状

单页应用里用 `<Transition>` 切换路由/视图，动画结束后，会短暂闪出一帧**上一个页面**的内容，
然后才稳定显示新页面。特征：

- 用 `npm run dev` 在 Chrome / Firefox 里看，**完全正常**；
- 打包成 `.deb` / AppImage 跑 WebKitGTK，**每次切换都闪**；
- 动画本身是流畅的，问题只出现在"动画结束的那一帧附近"；
- 用 Vue 的 `<KeepAlive>` 缓存页面时更明显。

关键判据就是第一条：**浏览器正常、打包后异常**，说明不是 CSS 写错了，而是 WebKitGTK
的合成/层叠行为与 Chromium 不同。不要再往 CSS 里堆 `z-index`，先按下面三条定位。

## 三个独立成因（可以同时存在，要逐个排掉）

### 1. `<KeepAlive>` 会在重新激活时重放一次 enter 钩子

这是"动画结束后"闪的**直接原因**。

Vue 的 `<KeepAlive>` 缓存组件后，再次显示该组件时会重新跑一遍 transition 的 enter 钩子
（`activate()` → `transition.beforeEnter()` / `enter()`）。于是：

```
第 1 帧  enter 开始，opacity 0
…        淡入到 1，动画看起来结束
第 N 帧  KeepAlive 激活，enter 钩子重放 → opacity 又被设回 0
第 N+1 帧 再淡入一次
```

肉眼看就是"动画结束 → 闪一下 → 恢复"。我实测的逐帧不透明度序列（60fps 采样）：

```
0.9999 → 0.99999 → 0.1879 → 1     ← 掉回 0.19 再回来
```

**修法：把 opacity 从 enter 过渡里拿掉，只保留 transform。**

```css
/* 不要这样：KeepAlive 重放时会把屏幕打回全透明 */
.view-enter-from {
  opacity: 0;
  transform: translateY(6px);
}

/* 这样：重放时 transform 从 6px 开始，肉眼无法与 0 区分 */
.view-enter-active {
  transition: transform var(--dur-3) var(--ease-out);
}
.view-enter-from {
  transform: translateY(6px);
}
```

如果确实需要淡入效果，就**不要用 `KeepAlive` 缓存那个视图**，或者给过渡加一个
`v-if` 之外的一次性标志位，让重放走空操作。

### 2. 定位与层叠写在 `-enter-active` 上，会在旧节点卸载前失效

Vue 的移除顺序是：**先摘掉 enter 类，下一帧才移除旧节点**。如果定位/层级写在
`-enter-active` 里：

| 时刻 | 新视图 | 旧视图 | 谁在上面 |
| --- | --- | --- | --- |
| 切换中 | `position:relative; z-index:1` | `position:absolute; z-index:0` | 新 ✅ |
| enter 类已摘、旧节点还在 | `position:static`（回落） | `position:absolute`（还在） | **旧** ❌ |

因为**定位元素永远画在非定位元素之上，与 z-index 无关**。这一帧就是"旧页面盖回新页面"。

**修法：把盒子与层级写在常驻的元素规则上，只在 leave 时降级旧视图。**

```css
/* 常驻：两个视图永远是同一层的绝对定位盒子 */
.stage > .view {
  position: absolute;
  inset: 0;
  z-index: 1;
}

/* 只有"正在离开"的那个降下去 */
.stage > .view.view-leave-active {
  z-index: 0;
  pointer-events: none;
}
```

⚠️ 如果用 Vue 的 `<style scoped>` 给 `.view` 写规则，scoped 会生成
`.view[data-v-xxxx]`（特异性 0,2,0），**会压过全局的 `.view-leave-active`（0,1,0）**，
导致 `z-index: 0` 静默失效。要么把盒子规则也放进全局样式表，要么用 `.stage > .view`
这种更高特异性的选择器（本项目最后的写法）。

### 3. `transform` 动画结束时合成层被销毁，整屏重绘

WebKitGTK 会为带 `transform` 的元素建一个合成层。动画结束后 `transform` 计算值回到
`none`，这一层就被销毁，WebKit 重新栅格化整块内容——在 AMD/Mesa 上很容易吐出一帧旧的
tile。这也是"只在打包后出现"的原因之一。

**修法：让元素常驻一个"恒等 transform"，图层就一直活着。**

```css
/* :where() 保证特异性为 0，不干扰 -enter-from 的覆盖 */
:where(.stage > .view) {
  transform: translateY(0);   /* 而不是 none */
}
```

### 4. 小面板的进场动画会在"重挂载"时被重放，且发生在动画结束很久之后

成因 1 说的是 KeepAlive 重放 enter 钩子；实测发现范围比那更宽：**任何**带
`opacity` / `transform` 的进场 `transition` 或 `animation`，在元素被 KeepAlive
重新插入时都会被 WebKitGTK 再播一次——即使该动画在几百毫秒前就结束了。

证据（`/tmp/wk/panelprobe.py`，8 ms 采样 `getComputedStyle` + `elementsFromPoint` +
`className`）：登录面板已经稳定在 `translateY(0)` 数帧之后，`transform` 突然跳回
`translateY(≈8.7px)` 再回到 0，而 `className` 全程没有变化：

```
t=1909  op=1.000 tf=matrix(1,0,0,1,0,0)      cls=panel
t=1913  op=0.996 tf=matrix(1,0,0,1,0,8.68)   cls=panel   ← 重放
t=1920  op=1.000 tf=matrix(1,0,0,1,0,0)      cls=panel
```

`cls` 不变说明不是我们的状态类切换，是引擎自己重播了样式里声明的动画。

**修法：小面板干脆不做进场动画**，只留常驻的恒等 transform 保住合成层。

```css
.panel {
  /* 故意没有进场 transition / animation：WebKitGTK 会在 KeepAlive 重新插入时
     重放它，且发生在动画结束很久之后，看起来就是"页面闪一下"。 */
  transform: translateY(0);
}
```

修完同一探针输出 `missingFrames: 0, opacityDips: 0`，全过程只有 5 帧变化且全部
`opacity=1, translateY=0`。教训：**在 WebKitGTK + KeepAlive 组合下，"进场动画"
不是免费的**——它会在你意想不到的时刻再播一次。视图切换那种大面积的过渡仍值得做
（配合前三条），但小面板、弹层、提示条一律不做。

### 5. CSS transition 驱动的全屏/大面积动画，结束时仍会整内容区重绘一帧

前四条修完之后，视图切换仍有概率性单帧异常（约 1 次 / 8 次切换）。逐帧像素 diff 定量
之后才看清：**问题不在 easing、不在 `scale`、不在 `backdrop-filter`，而在
`transition` 本身的合成层生命周期**。

诊断脚本（`/tmp/wk/`，Python + WebKitGTK 4.1 GI，`get_snapshot(VISIBLE)` + numpy）：

- `diffbox.py`：相邻帧 diff 的 px / pct / bbox，用来区分「局部重绘」和「整屏重绘」。
- `outchar.py`：单帧 outlier 的数值特征（`agrees_prev` / `agrees_next` / `unique_colors`）。
- `vslast.py`：每帧与**最后一帧**比较，只有被两侧「等于终帧」的帧夹住的单帧差异才算
  肉眼可见的 glitch——这一条把「内容正确的重绘」和「真的闪了」分开。
- `many.py`：一次运行里连点 8 次导航，统计所有 outlier，比单次触发可靠得多。

数字（视图切换，8 次/运行）：

| 写法 | 异常帧 / 运行次数 |
| --- | --- |
| CSS `transition` + `translateY(6px)` | **11 / 10** |
| `:where(...) { transform: translateY(0) }` | 仍有 |
| `translateZ(0)` / `will-change` / `contain: paint` / `isolation` | 0–4，不稳定 |
| 动画挪到子元素 | 7 / 8 |
| 缩短 duration | 13 / 10 |
| `@keyframes` 重放 | 17 / 6（更糟） |
| **WAAPI `element.animate()`** | **0 / 18** |

**修法：把这类动画交给 Web Animations API，并给 `fill: "both"`。**

```ts
const anim = el.animate(
  [{ transform: "translateY(6px)" }, { transform: "translateY(0)" }],
  { duration: 300, easing: "cubic-bezier(0.22, 1, 0.36, 1)", fill: "both" },
);
anim.finished.then(done, done);
```

```vue
<Transition name="view" :css="false" @enter="viewEnter">
```

`fill: "both"` 不是可选项：没有它，WebKitGTK 会在动画结束瞬间拆掉合成层并整内容区重绘
一次（实测 19 次异常 / 10 次运行）；加上之后是 0 / 10。`done()` 必须挂在 `finished` 上
（并且 `.then(done, done)` 兜住 reject），否则 `Transition` 不会进入下一阶段。

同理，**覆盖整个 viewport 的层不要做 `opacity` 过渡**：`Modal` 的遮罩改成瞬现、只让
面板本身淡入之后，打开时的整屏重绘从 `243 322 px` 降到 `36 236 px`（只剩面板自己的
bbox）。反面证据：`*{backdrop-filter: none}` 与 `*{box-shadow: none}` 都仍有 `243K px`，
说明这两者不是原因，不必为了它牺牲毛玻璃效果。

判据小结：**如果 `vslast.py` 报不出 GLITCH，说明那一帧的内容是对的，用户看不到**——
`diffbox.py` 的大数字只能说明「重绘了」，不能说明「闪了」。

### 6. 小面板（下拉菜单 / 气泡）的 CSS `transition` 在 WebKitGTK 上根本不是逐帧推进的

这一条和第 5 条成因相同（CSS transition 的合成层生命周期），但表现完全不同，也更难
察觉：**小面板不会闪一帧，它会卡住不动，然后在结尾一跳**。用户对它的描述是「抽搐」
（`m11331`：「还是会有抽搐，主要集中在选择网盘的菜单打开和关闭的时候」）。

诊断方法（`/tmp/wk2/`，同一套 Python + WebKitGTK GI 探针）：

- `uniq3.py`：一次 open/close 期间连拍，按 md5 去重，输出「唯一帧序列」和相邻唯一帧
  的最大间隔 `maxgap`。**`maxgap` 是关键指标**——`uniq` 数量少 + `maxgap` 大就是卡顿。
- `out2.py`：先等菜单挂载后读它的 `getBoundingClientRect()`，把它（连同 28px 阴影
  衰减区）遮掉，只统计**区域外**的变化像素，用来判断「这次重绘是不是菜单自己引起的」。
- `swstyle.py`：用 rAF 采样 `getComputedStyle(el).transform/opacity`，确认动画值本身是否
  单调推进（`transform_replays` 必须为 0）。

`.switcher-menu` 的实测（打开，`(ms, 与终帧的像素差)` 的唯一帧）：

| 写法 | 唯一帧 | `maxgap` |
| --- | --- | --- |
| CSS `transition`（原状） | 3 | **210–219 ms** |
| 去掉 `will-change` / `contain` | 3 | 193 ms |
| 只过渡 `opacity` | 3 | 217 ms |
| 只过渡 `transform` | 2（关闭时仍卡） | 232 ms |
| **WAAPI `element.animate()`** | **10–11** | **25–46 ms** |
| WAAPI 只动 `transform` | 9 | 38 ms |
| WAAPI 只动 `opacity` | 4 | 206 ms |
| `transition: none` | 2 | 44 ms |

也就是说：`transform` 与 `opacity` **两个属性都必须走 WAAPI**，只把一个搬过去仍然会卡
（`opacity` 单独走 WAAPI 时 `maxgap` 还是 206 ms）。`box-shadow` 的大小完全无关
（`shadow-sm` / `md` / `lg` / 自定义 24px 60px 都测得 11–12 帧、`maxgap` 25–57 ms）。

**修法**：把 popup 的动画统一放进 `src/lib/motion.ts`，组件里用
`<Transition :css="false" v-bind="pop">`：

```ts
export function popEnter(el: Element, done: () => void) {
  if (prefersReducedMotion) { done(); return; }
  const anim = (el as HTMLElement).animate(
    [{ opacity: "0", transform: "translateY(-4px)" },
     { opacity: "1", transform: "translateY(0)" }],
    { duration: 190, easing: "cubic-bezier(0.22, 1, 0.36, 1)", fill: "both" },
  );
  anim.finished.then(done, done);
}
```

`fill: "both"` 同样是必需的（理由见第 5 条）。CSS 里只保留
`:where(.switcher-menu, .account-pop, .menu) { transform: translateY(0) }`，
`.pop-enter-*` / `.pop-leave-*` 规则要删掉——`:css="false"` 之后它们不再生效，留着只会
误导后来的人以为改 CSS 就能调动画。

落地后的实测（同一台机器、同一批探针）：

| | 修改前 | 修改后 |
| --- | --- | --- |
| 打开唯一帧 / `maxgap` | 3 / 219 ms | 10–11 / 26–46 ms |
| 关闭唯一帧 / `maxgap` | 2–3 / 96–216 ms | 6–7 / 24 ms |
| 菜单外重绘（`out2.py`） | 170–451 px | 441–451 px（同一量级，本来就不是问题） |

注意最后一行：**菜单外那点重绘并不是抽搐的原因**。早前用「菜单区域 + 190px」当遮罩
统计时看到每次 21k–43k px 的区外变化，误以为是整块内容区被重建；换成按菜单实际
`getBoundingClientRect()` 精确遮罩后，区外只剩几百像素——那 21k 其实是**菜单自己的
阴影衰减区**被算到了「区域外」。做这类统计时遮罩必须按元素真实 bbox 加余量，不能按
按钮 bbox 估。

## 推荐的完整写法

三层配合，缺一个都还会闪：

```css
/* 1) 常驻盒子 + 常驻恒等 transform（合成层不销毁） */
.stage > .view {
  position: absolute;
  inset: 0;
  z-index: 1;
  background: var(--bg);
}
:where(.stage > .view) {
  transform: translateY(0);
}

/* 2) 只给进场做 transform 动画（KeepAlive 重放时不可见） */
.view-enter-active {
  transition: transform 300ms cubic-bezier(0.22, 1, 0.36, 1);
}
.view-enter-from {
  transform: translateY(6px);
}

/* 3) 离场只降层级，不做任何视觉变化 */
.stage > .view.view-leave-active {
  z-index: 0;
  pointer-events: none;
}
```

配套的模板：

```vue
<main class="stage">
  <Transition name="view">
    <KeepAlive>
      <component :is="currentView" :key="view" class="view" />
    </KeepAlive>
  </Transition>
</main>
```

## 排查方法论（可复用到任何 WebKit 问题）

不要靠肉眼猜，把每一帧的状态打出来。在 WebView 里跑这段，切换几次视图看输出：

```js
const stage = document.querySelector('.stage');
let last = '';
setInterval(() => {
  const r = stage.getBoundingClientRect();
  const top = document.elementsFromPoint(r.left + r.width / 2, r.top + r.height / 2)
    .filter((el) => el.classList.contains('view'))
    .map((el) => `${el.dataset.screen}:z${getComputedStyle(el).zIndex}:op${getComputedStyle(el).opacity}`);
  const sig = top.join(' > ');
  if (sig !== last) { console.log(performance.now() | 0, sig); last = sig; }
}, 8);
```

- 看到 `op` 掉回小数值 → 成因 1（KeepAlive 重放）。
- 看到旧的 `.view` 出现在栈顶 → 成因 2（层叠）。
- 两者都正常还闪 → 成因 3（合成层），加恒等 transform。

注意 `pointer-events: none` 会让离场视图从 `elementsFromPoint` 里消失，排查时临时加上
`.view-leave-active{pointer-events:auto !important}` 才能看到它。

### 不要在 Wayland 下用 ffmpeg 抓帧

```bash
# Xwayland 的窗口内容是黑的，抓不到真实渲染
ffmpeg -f x11grab -i :0 ...
```

Wayland 下 `grim` 又要 `wlr-screencopy`，KDE 没有。要么用 `spectacle -b -n -f -o out.png`
（KDE 自带，走 portal），要么干脆不做像素级验证——**上面的 DOM 状态采样比截图更准**，
因为它直接反映浏览器内部的状态机。

## 附录：同一批项目里另外 8 个"只在 WebKitGTK / 打包后"才炸的坑

排查过程中顺手踩到的，都是通用问题：

### A. `blocking_read()` 在异步运行时里 panic

```
thread 'tokio-rt-worker' panicked at src/state.rs:128:
Cannot block the current thread from within a runtime.
```

任何 `tokio::sync::RwLock/Mutex` 的 `blocking_read()` / `blocking_write()` 都**不能**在
tokio 任务里调用。托盘菜单回调、系统通知、事件监听这些地方看起来"像是同步代码"，
实际上可能已经在运行时线程上。

修法：给需要跨线程读的少量设置用 `AtomicBool` 之类的原子量做镜像，
在 `update_settings()` 里同步更新。

```rust
pub struct AppState {
    pub notifications: AtomicBool,   // 任何线程都能安全读
}

pub fn notifications_enabled(&self) -> bool {
    self.notifications.load(Ordering::Relaxed)
}
```

### B. `tokio::spawn` 在 Tauri 的 `setup()` 里 panic

```
thread 'main' panicked at src/transfer.rs:208:
there is no reactor running, must be called from the context of a Tokio 1.x runtime
```

`setup()` 跑在主线程，此时 Tokio reactor 还没进入。**用 `tauri::async_runtime::spawn`**：

```rust
pub fn start_persister(self: &Arc<Self>) {
    let manager = Arc::clone(self);
    tauri::async_runtime::spawn(async move {   // 不是 tokio::spawn
        loop { tokio::time::sleep(Duration::from_secs(2)).await; manager.persist().await; }
    });
}
```

### C. 托盘 / 通知插件需要显式打开 feature

```toml
tauri = { version = "2", features = ["tray-icon", "image-png"] }
```

不开 `tray-icon` 会报 `could not find tray in tauri`；不开 `image-png` 则没有
`Image::from_bytes`。另外 `capabilities/default.json` 里要加 `notification:default`
和 `core:window:allow-hide/show/set-focus`，否则前端调用被静默拒绝。

托盘图标建议 `include_bytes!` 编进二进制，别用运行时路径：

```rust
const TRAY_ICON: &[u8] = include_bytes!("../icons/32x32.png");
```

### D. 关窗即退出会杀掉后台传输

托盘常驻要在 `on_window_event` 里拦 `CloseRequested`，同时用一个全局标志区分
"用户点了托盘退出"和"用户点了窗口 X"：

```rust
static EXITING: AtomicBool = AtomicBool::new(false);

.on_window_event(|window, event| {
    if let WindowEvent::CloseRequested { api, .. } = event {
        if state::is_exiting() { return; }      // 托盘"退出"放行
        api.prevent_close();
        let _ = window.hide();                  // 否则只是藏起来
    }
})
```

### E. 桌面项只注册一个自定义协议，浏览器"打开客户端"按钮无效

夸克网页的"打开客户端"会先探测 `http://127.0.0.1:9125..9130` 上的 `desktop_*` 接口，
探测不到才退化成"下载客户端"。所以光注册 `x-scheme-handler/xxx` 不够：

```ini
MimeType=x-scheme-handler/yunyu-drive;x-scheme-handler/qklink;x-scheme-handler/qkclouddrive;
```

再加上一个 loopback HTTP 服务应答 `/desktop_info`、`/desktop_caller?deeplink=…`。
注意浏览器从 `https://` 页面访问 `http://127.0.0.1` 属于私有网络请求，
响应要带 `Access-Control-Allow-Private-Network: true` 和 `Access-Control-Allow-Origin: *`，
并且处理 `OPTIONS` 预检。

### F. 只给主窗口拦 `CloseRequested`，别把登录 webview 一起藏起来

D 的写法有个陷阱：`on_window_event` 对**所有**窗口生效。登录用的 webview 点 X 时
也会被 `prevent_close() + hide()`，窗口对象永远不销毁。于是：

- 抓 Cookie 的轮询用 `get_webview_window(label).is_none()` 判断"用户关了窗口"，
  隐藏窗口永远存在 → 永远等下去，界面卡在"等待登录"；
- 再次点登录时 `if let Some(existing) = app.get_webview_window(label) { show() }`
  会把那个**陈旧的** webview 复活，它的页面和网络栈早已失效，表现为"内置浏览器
  连不上网络"。

```rust
.on_window_event(|window, event| {
    // 只有主窗口可以缩到托盘；登录窗口必须真的销毁。
    if window.label() != "main" { return; }
    ...
})
```

配套：webview 真被销毁时后端要发一个事件（本项目是 `登录窗口已关闭`），前端收到后
清掉"等待登录"状态，否则用户会看到一个永远不会结束的转圈。

### G. 身份字段不能为空：路由依赖它

139 云盘的 `get_personal_cloud_host` 用 `userInfo.accountName` 决定请求发到哪个
主机。我们用只带 token 的方式登录时手搓了一个 `Config`，`account` 留空 → 所有请求
都路由失败，报"已保存凭据但连接失败"。修法是让库自己解析 token
（`cloud139::client::auth::login`），失败再从 token 里 base64 解出
`pc:<account>:<tokeninfo>` 的第二段兜底。

教训：**"登录成功"和"能用"是两件事**。登录后立刻做一次真实调用（列根目录、
取用户信息）并把失败报出来，比返回一个 `authenticated: true` 的假成功有用得多。

### H. `<KeepAlive>` 里的页面拿不到新的 props/路由参数

症状：侧边栏明明选的是「阿里云盘」，点「去登录」却显示 123 云盘的账号密码表单。

原因不在登录页，在缓存。`App.vue` 是这么写的：

```vue
<Transition name="view">
  <KeepAlive>
    <component :is="currentView" :key="app.view" class="view" />
  </KeepAlive>
</Transition>
```

`key` 是**视图名**，所以四个盘共用同一个 `login` 缓存实例。`LoginView` 在
`<script setup>` 顶层用 `ref(app.loginTarget ?? app.activeProvider)` 算了一次
`target` —— 这段只在**首次挂载**时跑，之后每次进登录页都是把同一个实例重新激活，
`target` 永远停在第一次那个盘。

这跟第 1 条同源：`<KeepAlive>` 的实例只创建一次。**任何从模块级 store 读进来、
并假定"每次进入都会重新求值"的状态，在 `<KeepAlive>` 下都是错的。**

修法：首次挂载照旧，之后在 `onActivated` 里重新求值，并清掉上一个盘留下的表单。

```ts
const target = ref<ProviderId>(takeTarget() ?? app.activeProvider);
let firstActivation = true;          // 首次 activated 紧跟挂载，不要重复求值

onActivated(() => {
  if (firstActivation) { firstActivation = false; return; }
  const next = takeTarget() ?? app.activeProvider;
  if (next === target.value) return;
  target.value = next;
  credential.value = "";
  username.value = "";
  password.value = "";
  error.value = null;
});
```

顺带两条同样源于"组件一直活着"的规矩：

- `onDeactivated` 里要收掉离开时不该继续跑的东西。登录页的 webview 监听器和
  阿里二维码轮询都属于这类 —— 后台每 2 秒打一次 passport，会把还在等待扫描的
  二维码提前问过期。
- 页面级 `onBeforeUnmount` 里的清理**不能**指望它在切视图时执行；`<KeepAlive>`
  不卸载组件，只停用。

## 一句话总结

> 浏览器正常、打包后闪烁 → 先怀疑 **KeepAlive 重放 enter 钩子**（把 opacity 移出过渡），
> 再怀疑 **定位/层级随 enter 类消失**（把盒子规则常驻），
> 再怀疑 **合成层销毁**（常驻恒等 transform），
> 小面板/弹层干脆**不做进场动画**（引擎会在重挂载时重播它）。
>
> 状态对不上（显示了别的盘/别的页）→ 先查 `<KeepAlive>` 缓存实例里那次性的
> `ref(...)` 求值，改用 `onActivated` 重算。

四条都做，且用 DOM 逐帧采样验证，别用截图。
