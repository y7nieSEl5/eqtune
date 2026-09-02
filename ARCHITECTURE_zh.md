# eqtune

[eqtune on crates.io](https://crates.io/crates/eqtune)

## why eqtune

众所周知，Mac扬声器的原生调教非常保守，特别是在MacBook Air的丐版扬声器中表现明显，表现为中频率音量相对较大进而导致播放音乐时有扁平感。

现有的第三方equalizer都倾向于通过安装loopback/kernel drivers来替换掉原生扬声器。这会导致项目体型庞大，且在连接蓝牙耳机或有线耳机后，并不会及时切换输出设备。

eqtune通过利用Apple从macOS 14.2开始支持的**Core Audio process-tap API**来实现对系统级别音频的调衡。

## general 

```text
   you ── eqtune on/off/band/… ─▶    ┌─────────────────────┐
   (CLI client, short-lived)         │  thin client        │
                                     └──────────┬──────────┘
                                                │  one JSON request / reply
                                                │  over a Unix domain socket
                                                ▼
   launchd ── runs at login ──▶      ┌─────────────────────┐
   (KeepAlive)                       │  daemon (long-lived)│  owns config + audio engine        
                                     └──────────┬──────────┘
                                                │  Rust → C FFI (tap_shim.h)
                                                ▼
                                     ┌─────────────────────┐
                                     │  Objective-C shim   │  Core Audio / Foundation
                                     └──────────┬──────────┘
                                                │  process-tap API
                                                ▼
   system audio ─▶ device-scoped tap ─▶ aggregate device ─▶ IOProc ─▶ default output
                                         (capture → EQ → replay, one shared clock)
```
> 是的，这个项目没能完全用Rust实现：因为process-tap API更偏Objective-C形态，且目前缺少成熟的Rust封装🤷

`eqtune daemon`是一个隐藏的，在开机时即一直运行的进程。
而其它的命令，如（`on`, `off`, `band`, `preset`, ...）都是一个用于打开socket的client，它在发送一条指令并打印出返回结果后就退出了。

## modules

| File | Responsibility |
|------|----------------|
| `src/main.rs` | CLI parsing |
| `src/ipc.rs` | Control: `Request`/`Response`/`Status` enums, socket path, send/recv. |
| `src/daemon.rs` | long-lived process. 包括config, engine, 对engine的lifecycle的控制等 |
| `src/dsp.rs` | 信号处理: RBJ biquad设计, preamp实现, 用于防止过于激进的调教造成扭曲的`soft_clip`, 和实时的`Processor`. |
| `src/sys.rs` | Raw FFI到Objective C转换，外加一些safety wrappers (`TapSession`, `EqHandle`) |
| `src/config.rs` | 用TOML保存自定义调教: presets (bands和preamp) 和一些全局开关 |
| `src/launchd.rs` | LaunchAgent的安装和移除 |
| `shim/tap_shim.{h,m}` |Objective-C Core Audio shim, 以C ABI的形式暴露给项目的Rust部分 |
| `build.rs` | 编译shim，嵌入了`Info.plist`. |

TLDR：`src/sys.rs`和shim里面集中了没法优雅故障恢复、带`unsafe`、且仅macOS需要的代码。

## two planes

### control plane

`src/ipc.rs`中实现了用户命令与engine交互的途径：

```rust,ignore
enum Request  { Status, Enable, Disable, ListPresets, SetPreset(String),
                SavePreset { name }, ClonePreset { source, dest },
                DeletePresets { names }, RenamePreset { from, to },
                ExportPreset { name, path }, ImportPreset { path, name },
                SetBand { kind, freq, gain_db, q }, RemoveBand { freq },
                SetPreamp(f32), SetPreampAuto, GetResponse, SetBypass(bool),
                SetAutoOffLowPower(bool), SetAutoOffIdle(bool),
                SaveSessionAs { name }, SaveSessionOverwrite, DiscardSession,
                ResetPreset { name }, ConfirmResetPreset { name, backups },
                Reset, ConfirmReset { backups } }
enum Response { Ok, Status(Status), Tuning(Tuning), FrequencyResponse(…),
                BandRemoved { tuning, removed }, Presets { … },
                ResetWouldOverwrite { names },
                UnsavedSession { tuning, dirty_presets }, Error(String) }
```

一个client（比如`eqtune band 2000 -6`这行命令）会把一个`Request`改写成JSON，并把一行写入`~/Library/Application Support/eqtune/eqtune.sock`，再读到一个返回的`Response`。
daemon的接受循环（`Daemon::run`）处理每个连接。它读取JSON命令，改变状态，然后回复。
`Enable`下，会回复`Tuning`，让CLI打印当前调教曲线；`Disable`下，如果存在未保存的实时改动，会返回`UnsavedSession`并由CLI继续询问保存/覆盖/丢弃；若没有未保存改动，才返回`Ok`。`UnsavedSession`除了当前调教还带有所有实际存在未保存编辑的preset名单——编辑在切换preset后仍然留在原preset上，所以名单可能包含非当前preset；CLI会把它们列出来，而不是只显示当前曲线。
`RemoveBand`只有在输入frequency与一个已有band的配置frequency足够接近时才会删除一个band；否则不会改变调教，并会返回最近的已有frequency。成功时response也会带回实际被删除的band，让CLI可以如实显示。
如果任何后续prompt在用户作出选择前遇到EOF，CLI会报错退出而不会把EOF当作默认选项，也不会发送保存、覆盖、丢弃或确认reset的请求；未处理的session draft会继续保留。
另外，`export`命令会导出单preset TOML并返回`Ok`；而保存/克隆/重命名/import等命令会根据语义返回`Tuning`或预设列表，而不都是`Ok`。
`GetResponse`与`SetPreampAuto`共用实时RBJ coefficient的response计算，并使用已经验证的运行中输出sample rate；engine停止时则只解析一次完整的默认输出snapshot。JSON/CSV格式化和写文件都留在短命CLI里。`SetBypass`只改变内存中的runtime状态。

因为读写形式严格遵循输入一行JSON再输出一行JSON的规则，这个交互方式扩展和测试的成本都很低，也不会产生一些长时间运行的进程带来的莫名其妙的问题。

daemon对这行JSON也有硬边界：一次请求必须在总计5秒内读完，且不能超过64 KiB。这个检查会在每次读取后执行，包括读到结尾换行符的那一次，因此沉默连接、慢速滴字节、或一直不发换行的client都不能卡住单线程的accept/poll循环。
daemon在接触control socket之前会先获取一个nonblocking advisory lock，所以第二个daemon会直接退出，不能替换第一个daemon的socket或与它争用config和Core Audio状态。对于旧版本留下的socket，启动过程也会先探测，只会删除已经确认失效的Unix socket。

`Status`刻意保持为扁平、低频的control-plane快照，而不是callback telemetry。它会区分用户想要的on/off和engine实际是否运行，并给出挂起原因、输出UID/name/rate/stream facts、最后一次engine错误、有界retry状态、bypass状态和所有dirty preset。输出信息来自已经成功运行的target；启动失败时则保留最近一次完整的尝试snapshot用于诊断，但不会把engine误报为running。

### audio plane

Apple提供的**Core Audio process-tap API**允许一个来自user-space的进程获得系统的audio mix。eqtune利用这一点设置了三个对象：

1. device-scoped process tap

这个tap绑定到同一次snapshot得到的输出UID和唯一output stream，捕获所有发往该stream的进程音频，但不包含eqtune自己的进程，否则重新播放的音频会被再次捕获，造成反馈循环。Core Audio会让tap匹配所选stream的格式，因此44.1 kHz设备会得到44.1 kHz tap，不需要resampler。
另外，我们用`CATapMutedWhenTapped`来实现只在我们悄悄截走了音频的时候才把原生音频静音，否则关闭了daemon，原生的音频也没了。

2. private aggregate device

启动时只解析一次默认输出设备ID，再枚举这个设备的output streams。目前接受一个mixable、interleaved stereo Float32 stream，并使用设备原生sample rate（包括44.1和48 kHz）；UID、name、nominal rate和stream facts都按这个确切ID查询。相同UID和stream index用于构造tap，aggregate再把该输出（clock和playback）与tap绑定，避免设备切换竞态和额外resampling。通过验证并成功启动的target才代表running engine；完整但失败的尝试snapshot仍会显示在`status`中，方便诊断。

3. I/O callback

`AudioDeviceCreateIOProcID`和`AudioDeviceStart`中，每一个循环里，`io_proc`把系统音频导入output buffer，再调用Rust部分中的`eqtune_process_cb`来原位调衡那个部分的音频。

> daemon每100ms轮询一次默认输出设备和其sample rate。当你插入有线耳机或连接蓝牙耳机时，它会拆掉当前aggregate，并围绕新设备重建aggregate device。

IOProc每个block还会检查实际input/output buffer topology。如果layout或buffer size在运行中变得不安全，callback只会原子地发布一次fatal error并把当前危险block静音；control loop在下一个tick里drop `TapSession`。由于`CATapMutedWhenTapped`只在tap存活时生效，原生音频会立刻恢复，而不会让eqtune无限输出零。

### dsp and lock-free hand-off

`src/dsp.rs`中，EQ采用RBJ [*Audio EQ Cookbook*](https://webaudio.github.io/Audio-EQ-Cookbook/Audio-EQ-Cookbook.txt) 中的系数设计。

请注意
`EqSettings`（音频线程中所需全部参数的不可变快照）和`Processor`（音频线程本地的filter状态）通过`Arc<ArcSwap<EqSettings>>`相连接。control线程通过一次原子指针交换发布新快照，audio线程在每个block中用`load()`读取快照，无需等待。**audio线程是lock-free的**，这很重要，因为实时音频处理中阻塞或等待互斥锁可能引发优先级反转和掉帧。
新的coefficients、preamp和limiter状态仍会在audio block边界直接生效。runtime bypass是唯一例外：它在10 ms内于dry/wet两个endpoint之间渐变。即使完全dry，wet filter仍继续推进，所以切回wet不会复活旧state；它不写config，也不会停止tap。真正省电、恢复原生路径仍使用`eqtune off`。
每个`EqSettings`在构造时都会带上唯一的generation stamp，且内部字段保持私有，所以audio线程根据generation判断是否需要同步新coefficients，而不是依赖`Arc`的堆地址；即使allocator复用了同一个地址，也不会漏掉一次实时调教更新。`Processor`在创建时也会按`MAX_BANDS`（64）为每个声道预留容量，所有添加、导入、加载preset的路径都会执行同一个上限校验，因此切换到更大的preset时也不会在audio线程重新分配内存。
`src/sys.rs`负责把它俩连起来。`process-trampoline`是shim调用的`extern "C"`函数。它加载当前设置之后在buffer中运行processor。`TapSession`拥有原生session，在`Drop`的时候可以停止音频，所以这顺便很好地实现了`eqtune off`，本质就是drop掉`TapSession`，而且这可以避免泄露Core Audio对象或者以错误的方式终止它们。

`benches/dsp.rs`只用标准库离线测量steady-state、持续静音、settings update、64-band上限和bypass transition；production callback里没有clock read或timing counter。

## engine lifecycle

这部分的存在是因为本人在试用这玩意儿的期间发现，equalizer是一个很费电的事情。正常来说，MacBook Air的续航允许我连续播放数小时音乐。但在一个阴雨连绵的下午，我惊恐地发现，听歌2小时，电量从50%直接叠到了20%。

于是，我需要在合适的时候让daemon自己停止运行engine。

- `engine_target_on`：engine现在该在运行
- `user_intent`：用户明确指令的on/off，存在内存里。自动挂起（低电量、静音）都以它为准。它在启动时用持久化的`config.enabled`初始化，之后每次`on`/`off`一进来就立刻更新——早于把它写盘的那一步。
- `config.enabled`：持久化的用户意图，daemon启动时恢复，所以重启或重新登录后仍会尝试满足你上次的on/off。实时reconcile读的是`user_intent`。`eqtune on`会立即设置意图并尝试启动；即使第一次启动失败也仍会持久化这个意图、保持原生音频并进入有界恢复。`eqtune off`则先清掉意图、取消恢复、无条件停止engine，再持久化——写盘失败会报错并可重试，但绝不会让音频继续被处理，也不会让之后的reconcile把EQ又开回来。
- `low_power`：MacBook在低电量模式下吗？都低电量模式了，就别调衡了吧。
- `idle_suspended`: 没有捕获到任何音频呢？没放音乐，engine运行着干嘛呢？
- `recovery`：当前故障incident的retry次数、deadline和是否耗尽；`last_engine_error`保留最近一次启动或runtime错误。

`reconcile()`会让实际engine状态与`engine_target_on`对齐：该开就启动，该关就停止。daemon启动时用持久化的`config.enabled`初始化`user_intent`（并遵循低电量模式策略）先reconcile一次。如果开启了idle自动关闭，这次恢复是"懒"的——engine先挂起，等`follow_idle_activity`真正检测到在放音频时才启动，这样开机/重启时没在放音乐就不会白白让tap空跑一段静音；没开idle自动关闭时没有resume探测可依赖，就直接恢复启动。启动或runtime失败后，engine保持关闭、走原生路径，并只会在1、2、4、8、16、30秒后各retry一次。六次用完后，普通reconcile不能偷偷增加尝试；只有明确的`eqtune on`、输出设备改变，或真正的低电量/idle policy resume才会重置这次incident的budget。

> 其实，为了省电，我也想办法让`Processor`的开支减小。现在的`Processor` (a) 只在调教generation改变时同步filter的coeffs; (b) 0dB被忽略，因此不消耗biquad; (c) 持续静音时跳过逐sample处理。

## persistence & packaging

- **Config** 全部是存放在`~/Library/Application Support/eqtune/config.toml`的TOML。
  我在多次尝试之后设置了bright，mellow和pro三种自带的默认调教，具体特征见README。
  加载config时会先验证每个preset是否能被实时engine安全运行：数值必须有限且在范围内，preset最多64个band。无法解析或无法运行的config会被移动到`config.toml.corrupt`或带编号的同名备份，然后用内置默认值继续启动，避免launchd KeepAlive反复重启同一个坏配置。保存config时会先写同目录临时文件、fsync、再原子rename并fsync目录，以降低崩溃或断电时截断正式config的风险。
  daemon拥有实时调教和上一个被保存的调教。band/preamp编辑只改变当前正在运行的config，`off`指令会出触发对实时调教的保存与否、命名、覆盖其它已存在调教等一系列行为。切换preset（`eqtune preset`）则是"选择"而非"编辑"：它像全局开关一样立即写入已保存的config，单独切换不会触发`off`时的保存询问。按名保存只消耗当前preset的编辑（对已有preset的显式覆盖也会取代该preset的待定编辑）；留在其它preset上的未保存编辑会被带入新的工作config，仍是一个未关闭的session，由下一次`off`继续询问，绝不会被悄悄还原。`preset-clone`与其它preset管理命令一样，在session未解决时会被拒绝——它会用已保存的config重建工作config，否则会丢掉这些编辑。
  当实时调教与已保存的config不一致时，实时调教还会被镜像到旁边的`session.toml`（同样的临时文件+原子rename，但省略config的fsync：镜像是尽力而为的，在单线程循环里做耐久级flush毫无收益；写入还会限速——静默一段后的第一次编辑立刻镜像（单个孤立编辑绝不会有丢失风险），短时间窗口内的连续编辑则合并成一次、由poll循环flush，所以拖动一个控件不会每动一下就重写整个config），session一解决镜像就会被删除；删除失败会作为错误上报，因为残留的镜像会在下次启动时复活刚刚解决掉的session。daemon启动时会把遗留的镜像恢复成"未保存的草稿"，所以重启、崩溃或重装不会悄悄丢掉你还没保存的实时改动，`off`时的保存/覆盖/丢弃询问照旧。镜像里只逐preset地信任已保存config里存在的preset的内容——合法的草稿只会修改既有preset，因此过期草稿既不能删掉刚保存的preset也不能夹带新preset；当前active preset和全局开关一律以已保存的config为准（它们都是立即提交的）。无法解析或无法运行的镜像会被移到`session.toml.corrupt`后忽略。
  这样的保存方式自然也就允许调教的import和export。为了减小文件尺寸，import/export使用一个更小的单preset TOML格式（只包含`name`, `preamp_db`和`bands`）。在CLI中，import/export相对路径默认按当前工作目录解析。

- **launchd** LaunchAgent plist配合`RunAtLoad`和`KeepAlive`，保证daemon在login时启动、挂了会被重启。`eqtune install`会把binary先暂存为同目录临时文件、在这份暂存副本上本地做ad-hoc签名、再原子rename到稳定位置，并在bootstrap或重启agent后确认launchd确实进入了running状态。已经健康加载的agent仍用`launchctl kickstart -k`原地重启；如果launchd残留了过期的启动约束、重启后的job起不来，install会退回到bootout+bootstrap。
- **无需Developer ID签名。** 安装时对daemon副本用本地ad-hoc签名，所以不需要Apple开发者账号、证书、公证、驱动或内核扩展。`build.rs`把`Info.plist`嵌进binary，让macOS弹出正常的音频捕获授权提示。

## FAQ

1. 为什么不用Apple自带的Equalizer？

macOS里的graphic equalizer只对Apple Music自己的播放有效。对Safari, Spotify, 视频、游戏、系统声音都不起作用。
而现有的第三方方案通常选择安装loopback/kernel audio driver，把它自己变成默认输出设备，所有声音都经过这个虚拟设备处理。这会破坏macOS的正常设备切换，需要内核扩展和随之而来的signing/notarization。

2. 为什么用Rust？

因为这是Rust程序设计这门课的大作业（bushi）

- Rust没有GC。对音频作出实时修改肯定不能接受突然apocalypse的GC存在。Rust的所有权也让**lock-free `ArcSwap` hand-off**非常容易实现。
- daemon需要长时间运行。臭名昭著的Microsoft Office全家桶已经造成了难以计数的内存泄露。Rust能保证这个项目的内存不安全部分只有`sys.rs`里面的一小部分代码。
- 我爱`cargo`

3. 为什么不能只用Rust？

DSP, config, IPC, daemon, lifecycle等都完全用Rust实现。唯一的Objective-C代码是shim/tap_shim.m，它存在是因为eqtune依赖的API只能从Objective-C/C调用。
这大约250行Objective-C代码用ARC(`-fobjc-arc`)编译，以确保内存管理的正确性，对外暴露一个很小的C ABI，而Rust通过`sys.rs`中的一小段`extern "C"`接口和它交互。
