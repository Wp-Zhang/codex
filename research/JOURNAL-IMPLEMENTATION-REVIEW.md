# Journal / host codec / strict framing：实现复审

日期：2026-09-28。工作区：`/Users/wpzhang/.codex/worktrees/panng-prototype/Codex-Panng`。范围：`panng-runtime/src/journal*.rs`、`history/src/panng.rs`、`rollout/src/strict_jsonl.rs` 与 native append preflight 接线。按本轮读取版本标行号；实现仍在补测试，后续修正应另记 disposition。

只读并写本报告，没有改源码或运行测试。源码存在不等于提交协议已经启用：读取时 PanngCommit 仍是 history 导出的独立 codec，没有作为新 RolloutItem / history mode 接入完整 runtime replay，strict scanner 也明确只负责只读字节分帧。

## Verdict

**当前结论：J1–J4、R1-I1–I3 均已静态确认修复并关闭；接受本阶段受限的codec + 只读suffix语义恢复接口。** 未发现阻止该小阶段继续的新实现问题。接受不包含writer reconcile、mutating tail repair、host publication或真实执行授权。主agent报告首轮相关209 tests通过；最新补充用例仍待其统一复测，本reviewer未运行测试。

对 upstream envelope 的新收窄建议：**接受 fail-closed 的 canonical round-trip 校验；不接受绕过 upstream 的 skip_deserializing 以强行恢复 warehouse-only 记录。** 详细条件见 J4。

## 最新修正复核 disposition

- **J2 关闭（静态）。** `strict_jsonl.rs:147–174` 改用流式header visitor；`:176–260` 只为key/discriminator设置小token预算，以IgnoredAny跳过其他大字段，不再将整个native header限制为256KiB。追加测试使用300KiB base_instructions，覆盖plain/compressed foreign拒绝且字节不变、native仍可Resume。源码满足本次兼容修复目标，测试尚未由本reviewer运行。
- **J3 部分关闭。** `panng.rs:74–98` 已要求Fork才有parent_cut、WorldState必须full、settings恰有一项、窗口ID非空。仍需将非空改为宿主可解析的UUIDv7：`core/src/session/rollout_reconstruction.rs:538–553` 只接受v7，当前`panng_tests.rs`用`window-0`通过codec，进入真实恢复会变None。first/current和存在时previous都应按该规则验证，fixture使用真v7。current settings与header thread身份的关联适合在R1语义reader/宿主构造边界再检查，不能仅凭event存在就视为绑定完成。
- **J4 修正确认（静态）。** `panng.rs:124–131` 对new的数据执行canonical往返并拒绝不可恢复字段；`:184–188` 的Deserialize保留raw Value并与重编码canonical Value比较，能发现skip_deserializing先吞掉的字段。新测试同时覆盖new及raw导入的cell_id拒绝，方向正确；其余warehouse字段建议表驱动覆盖。canonical builder仍需显式定义未来允许剥离的warehouse-only字段，当前codec没有静默删字段后成功。

- **J3 最后复核关闭（静态）。** 最新`panng.rs:82–90` 对first/current及存在时previous window ID执行UUID解析、version==7和canonical文本检查，修复了原非空字符串仍会在宿主恢复时变None的问题。fixture已替换为UUIDv7，`window-0`作为拒绝用例。此前settings/full-world/parentcut修正保持有效。此关闭不代表R1已经完成settings与header thread的语义绑定，见后文独立发现。

以下原发现保留为修复依据；后文新增R1分阶段接口审核。

## J1 · P1：逻辑调用去重键偏离已定契约——已静态确认修正

初读 `journal.rs:54` 用 `(origin_branch, turn_id, logical_step_id, call_id)` 作为重复键，遗漏 owner_epoch。这样同 epoch/step/call 只改 turn_id + OperationId 即可绕过去重，也会误拒不同 epoch 的同 call。

通知主 agent 后复读，当前键已改为 `(origin_branch, owner_epoch, logical_step_id, call_id)`。新增 `journal_tests.rs:129–148` 分别验证不能只改 turn 绕过，以及不同 execution epoch 可区分。**实现发现静态关闭；测试结果尚未由本 reviewer观察。**

这个修复与 immutable identity 检查互补：`OperationRecord::follows` 约束同 OperationId 的来源不变；logical key 则防止以新 OperationId 重复准入同一调用，二者不能替代。

## J2 · P1：新的 256 KiB header 上限破坏既有 native Resume

位置：`rollout/src/strict_jsonl.rs:10,148–175`；`recorder.rs:2037–2040`。

新 preflight 对每个 native Resume 都先读至多 256 KiB+1 的首行，超过 256 KiB 就拒绝，尚未读取 history_mode。既有 `SessionMeta` 的 base_instructions/dynamic_tools 在 history_mode 前序列化（`protocol/src/protocol.rs:3168–3181`），这些字段没有对应 256 KiB 类型上限。合法 native rollout 的首条 header 完全可能大于该值；此前 native loader 可以读取，现在被预检拒绝。

这是本轮实际接入造成的行为变化，不是只对未来 Panng 文件施加边界。不能以“安全起见”默默改变用户已有 native 会话格式的可读范围。

**修正建议：** native mode 探测使用有界内存的流式 JSON header visitor，跳过不相关的大字段，只提取 type/history_mode 并完整判断首条 header；不要把 Panng record 大小限制套在未知/旧 native header 上。对未知模式不能因为超出探测前缀就盲目放行。也可以实现独立、兼容既有大 header 的只读预检路径，重点是拒绝 foreign mode 必须早于 materialize/尾部补换行，同时 native 正常大 header 仍可读取。

**最小验证：** >256 KiB 的合法 legacy/paginated 首行仍能 Resume；同样大的 panng_v1/unknown 首行被 Unsupported 拒绝且原字节不变。plain 和 compressed 两种表示都覆盖，unknown compressed 不创建 materialized plain 文件。

## J3 · P1：joint checkpoint 构造器尚可接受 patch-only world baseline 或缺失当前 settings

位置：`history/src/panng.rs:65–90`。

构造器检查 replacement_history/resume_metadata/window_number 非空，以及 companions 的种类/不重复，但没有检查 WorldStateItem.full，也没有要求 ThreadSettingsApplied 存在。于是一个被当成完整 snapshot 的 record 可以只带 WorldState patch，或完全没有 companions。

`protocol/src/protocol.rs:3272–3284` 明确区分 full baseline 和依赖旧 baseline 的 patch。bounded replay 只从最新 snapshot 开始，patch 不能恢复 snapshot 以前的基线。缺失当前 settings 也不能默认由可选历史 TurnContext 补齐：它可能是旧 step 的设置，不能代表中途已应用的新配置。

**最小修正：** checkpoint 有 WorldState companion 时必须 full=true；不存在时明确定义为没有基线，不回退旧状态。要求一个 current ThreadSettingsApplied（与本 branch 的 canonical thread 身份匹配由适当层验证），TurnContext 可以按既有语义明确缺席。继续拒绝重复/未知 companion。window identity 的完整/有意缺席规则也应明确，不能只凭 window_number 就声称完整窗口快照。

这些是 snapshot 自包含所需的结构规则，不应全部延期到未来 core adapter。若将该类型定位为仅 syntactic DTO，就不能让 `is_checkpoint()` 单独成为 strict scanner 的安全停止依据；需要一个验证后 joint-checkpoint 类型供 reader 使用。

验证：patch-only companion、缺少 required settings、重复 settings 拒绝；显式无 world baseline 的合法快照通过并恢复为空；live/replay 的当前 settings 与 baseline 一致。后续 prior→next 运行时校验仍另行执行，不能只靠构造器。

## J4：拒绝不可恢复 upstream envelope 是正确收窄，需同时约束写入与读取

源码：`protocol/src/models.rs:975–993` 明确说明输入和 rollout reload 不能伪造 tool call records，因此 `cell_id`、`executed_tool_calls`、`tool_calls_complete` 使用 skip_deserializing。将原始 upstream ResponseItemEnvelope 直接 encode/decode 恒等并不是可实现的承诺，也不应为此修改安全语义。

主 agent 提议的顺序正确：

1. codec `new` 校验 canonical encode→decode→encode 稳定；遇会丢字段的输入明确拒绝，不替调用者静默剥离后返回成功。
2. 后续可信 host builder 显式沿用 upstream 持久化规范，将 warehouse-only 数据剥离，得到定义明确的 canonical history view。
3. runtime operation identity、结果来源、授权分类等真正需要恢复的事实，保存在自身受验证的 journal/host metadata 中；不能从重新接纳 warehouse-only 字段获取执行权或恢复“已证明的调用清单”。

**不能把这描述成“所有原始 metadata 均原封不动恢复”。** 正确承诺是“已定义的 canonical 可恢复视图不发生静默丢失”。字段剥离要有明确允许列表和 fixture；未知新字段不能自动剥离。ResultEncoding 的版本对应这个固定codec，未来上游变化通过兼容/迁移检查处理。

还有一个读取侧区别：若 `Deserialize` 先把 skip_deserializing 字段丢掉，再调用 new，new 的 round-trip 检查已看不见原字段。读取时若声称“遇有损 raw record 明确拒绝”，就需要从原始 JSON value/bytes 校验 canonical 对比或显式拒绝这些禁止输入字段。仅 writer constructor 校验可确保自己新生成的数据稳定，但不能自动覆盖旧记录/import。不要为补这个检查改变通用 upstream Deserializer 的安全忽略行为。

读取时尚未看到该 round-trip 补丁落入 `panng.rs`，因此本项为对修订方案的认可，不是代码通过记录。对应测试应包括三种 warehouse 字段各自出现、合法 turn_id/content_item_kinds/其他恢复metadata、含浮点checkpoint，以及 raw-import 中出现禁止字段。浮点 canonical 比较要依据固定序列化规范，不能因测试困难而丢弃业务字段。

## 已确认的正确实现方向与边界

### Runtime candidate 转移

- `journal.rs:128–156` 在 cloned map 上验证 revision/provenance/state，整体成功才返回新 snapshot，不修改 prior；后项失败不会把前项半发布。
- `:167–169` Compaction/ReplayBound 要求 state==old，当前实现禁止全部 pruning，覆盖已选择的当前 epoch 安全要求。
- `:170–192` Fork/ResumeFence 要求 branch/epoch变化符合原因，Prepared revision前进一步，Fork变InheritedUnknown，ResumeFence必须typed Indeterminate，已观察结果原样保留。
- 这些函数不验证外层 commit sequence/commit ID，也没有授予执行权；后续 writer/replay coordinator必须调用它们，不能只deserialize快照就认定迁移合法。
- 64项状态容量、每delta32项及未知结果类型限制均有校验。正式长任务的容量策略仍未完成，不应把本codec上限当作长期功能支持。

### Strict framing / native 早期拒绝

- `strict_jsonl.rs:94–125` 将未换行尾与完整行区分，完整坏JSON/空行报错；`:109–114` 在扩张前限制单条buffer；`:130–141` 在读新chunk前限制累计字节；错误后scanner保持failed，不静默跳过。
- preflight接在 `materialize_rollout_for_append` 和 native 补换行之前，方向正确。测试 `native_constructor_refuses_foreign_headers_without_repairing_tail` 已写明plain/compressed不改字节及native尾部保留原行为；本 reviewer未运行。
- scanner是只读framing，不验证Panng schema/branch/sequence，也不截断文件。新 codec尚未证明由它完整接管恢复，不应宣称strict reopen/reconcile完成。
- `new_at(end)` 可以表示历史冻结cut；该cut内的未终止尾不代表物理文件EOF可以修复。未来mutating repair只能在取得writer ownership并确认操作的是当前物理尾部时执行，不能把任意历史cut的TornTail拿去截断活文件。

### 本轮未闭合但明确未激活的接线

`PanngCommit` 文档明确writer还须验证 prior transition与实际UI投影cardinality；目前未看到对应完整writer/recovery路径。当前不暴露生产mode是正确做法。需要后续完成：stable commit-ID/sequence reconcile、真实strict重新打开、早期settings/token读取、UI page_size=1 cardinality、native与Panng入口模式隔离。不能通过为新variant加忽略分支获得“支持”。

## 验证优先级

1. J2大native header兼容与foreign模式不改磁盘。
2. J3自包含checkpoint与J4canonical provenance拒绝/允许矩阵。
3. Runtime logical key（J1）、snapshot reason与失败原子性测试；确认新增测试实际通过。
4. 实际 commit reader/writer接线后，再验证同ID不同payload、序列缺口、ack丢失re-sync、快照锚点+suffix等价及factory零抢跑。

本轮不需要全工作区测试；实际通过数由主 agent 的 runtime/history/rollout定向验证记录提供。

## R1 专属语义 reader 的分阶段接口审核

**认可暂时使用专属 outer Panng line parser，而不先扩展全局 RolloutItem。** 这样未接线native消费者不会仅因enum编译通过就忽略关键状态。native append preflight继续拒绝marked mode，R1仅返回验证后的只读数据，不repair、不发布历史、不构造executor、不宣布PanngV1能力。

主 agent 拟定 header 为 `payload.history_mode="panng_v1"`、`payload.panng={version:1,branch:UUID}`，payload.id绑定thread；header ordinal=0，首snapshot sequence=0/ordinal=1。该设计可以使用，但必须补齐以下条件，避免返回看似有效的错误恢复状态：

1. **根与最新锚点分别验证。** bounded tail找到后方snapshot不证明首snapshot存在。应单独有界读取header后的第一条完整commit，验证Initial/Fork、sequence=0、previous_sequence=None、ordinal=1及branch；若首commit缺失/不完整，拒绝。prefix读取预算与32MiB尾部预算明确区分，不能为证明根暗中全量扫描。最新锚点可处于更晚sequence，不能强迫它也为Initial。
2. **冻结读取终点及两种序号。** 初始取确定的文件end/cut，在此范围内读；不追逐正在增长的tail。若本R1文件语法只支持header+PanngCommit，直接要求每条outer ordinal=sequence+1且后缀连续。若允许夹杂native记录，则分别校验outer ordinal连续与runtime sequence连续，并明确允许的native类型；未知记录必须拒绝，不能直接跳过。建议R1先用较小的专属语法，未来宿主接入再显式扩展。
3. **完整后缀顺序验证。** 逆读找到self-contained checkpoint后，按正序验证每条delta的branch、前序sequence、commit identity、OperationChange revision及`RuntimeMutation::apply`。同ID不同内容拒绝；实际文件中的重复sequence/重复commit也不能当网络幂等请求轻轻忽略。读者不能只deserialize每行后取最后一个snapshot。
4. **header与宿主状态交叉绑定。** 所有commit.branch匹配header.panng.branch；header.id是被请求thread，或调用者必须在返回后明确核对。checkpoint的ThreadSettingsApplied.thread_id不能来自另一线程；缺少thread身份需要明确拒绝或定义受限补全，不能默默把任意settings应用到当前thread。Prepared操作必须属于当前branch/epoch；Ready/InContext可能是合法继承事实，不得据此授予执行权。
5. **锚点信任范围明确。** 晚期Compaction/ReplayBound/ResumeFence snapshot在bounded恢复中作为自包含锚点，不必也无法重新验证它之前的全部迁移；writer在产生snapshot时负责prior→next规则。R1应报告这个范围，不声称全文件无损。首Fork的parent_cut是来源声明；只有加载父冻结cut后才能声称验证了父子等价，R1单文件加载不能伪称完成这一点。
6. **TornTail不等于修复许可。** 只记录初始物理EOF的未终止tail长度及最后完整cut。任何完整坏行、版本错误、缺序或超限均返回Err，不返回“可修复成功结果”。如果输入是历史冻结cut而非当前EOF，落在行内必须视为非法cut，不能把它当物理尾截断建议。
7. **返回值保持只读语义。** raw header、checkpoint、deltas、恢复runtime和cut可用于下一阶段，但cut是“已读取完整记录的位置”，不是来自sync ACK的DurableRecordReceipt。Prepared状态不能被自动dispatch；需要后续真实writer ownership、ResumeFence/reconcile、完整host恢复和能力门才可继续工作。

R1尚不需要全局UI投影接线或生产feature，但必须拒绝没有语义支持的记录。将返回类型命名为恢复读取结果而非“可继续执行会话”，并保留明确scope文档即可维持阶段边界。

最小R1验收：header-only、缺首snapshot、首delta、错误ordinal、不同branch/thread、latest checkpoint+有效suffix、后缀缺seq/重复commit、完整坏末行、未换行末尾、历史cut落行内、超限record、非法window UUID，以及runtime应用后与完整有效日志重放一致。所有用例仍为只读，断言原文件不变。

## R1 suffix reader：实际实现复核

范围：最新 `rollout/src/panng_recovery.rs`、`panng_recovery_tests.rs`。只读审核，未执行测试。总体实现遵守专属parser、只读、不激活native能力的staging；以下三项需修正后再接受“验证后的语义suffix”接口。

### R1-I1 · P1：只读cut使用了SyncedRecord，类型声称了并不存在的durable ACK

`panng_recovery.rs:19,35,125–126` 直接构造并返回 `SyncedRecord`。该类型在同步原语中的契约是“同步且已确认后的canonical位置”；此reader仅读到newline-terminated记录，既没有调用sync，也没有取得writer receipt。正在运行的writer可能写完一行但尚未sync，此时reader仍可返回该类型。

最小修正：定义独立 `PanngReadPosition` / `JournalReadCut`，明确ordinal/end_byte_offset仅描述冻结读取范围中的完整行边界，不能作为DurableRecordReceipt使用。不要做类型alias。后续reconcile需要用writer重核对并sync既有记录，才可得到同步receipt。测试expected也换成只读位置类型。

### R1-I2 · P1：commit branch绑定通过，但host settings的thread身份未验证

`decode_commit`（`:168–174`）验证outer type、timestamp长度、ordinal和commit.branch；header.payload.id最终返回，但未与checkpoint或delta的ThreadSettingsApplied.thread_id交叉检查。现fixture `panng_recovery_tests.rs:31` 甚至用null作为正常值。

因此文件可以header声明线程A、commit属于正确branch，却携带线程B的current settings，仍被返回为有效checkpoint。后续宿主可能误用或忽略这些设置；codec要求“有settings”不足以证明“当前thread的settings”。

最小修正：R1语义验证拿到header thread ID，要求snapshot companion以及支持的settings delta中明确的thread_id与之相同；对marked格式缺失thread_id应拒绝或另行制定明确的受限补全规则，当前推荐要求Some(id)。更新fixtures为真实header ID，并测试null与foreign ID。调用者实际读取某个指定thread时，还需核对返回header.id等于被请求ID；独立文件reader没有requested-id参数不等于已完成这层校验。

### R1-I3 · P1：Fork/ResumeFence锚点仍能包含本不可能存在的Prepared

`:141` 对最新snapshot直接clone runtime作为锚点，随后只检查Prepared是否拥有当前branch/epoch。这符合不回读全部prefix的设计，但要求snapshot具有充分的自包含结构约束。

当前 `RuntimeMutation::validate` 仅检查Initial必须为空。于是可构造首Fork snapshot（带parent_cut）或后方ResumeFence snapshot，包含current branch/current epoch的Prepared。它通过`validate_prepared_ownership`，虽然按既定转移规则：Fork中的旧Prepared必须变InheritedUnknown；ResumeFence中的旧Prepared必须变Ready/Indeterminate，且不能在fence之前先创建新操作。这类snapshot不能由合法prior→next迁移产生。

最小修正：在runtime的snapshot结构validate中加入“Fork和ResumeFence不得含Prepared”，使R1载入锚点无需扫旧prefix也能拒绝显然不可能的状态。不要只在reader复制同一规则。Compaction/ReplayBound仍可包含当前branch/epoch的Prepared。新增用例要用**看似拥有当前epoch**的Prepared，现有foreign-owner测试不能覆盖。

该reader不执行Prepared，故不是已经发生的副作用重放；这是返回状态违反既定恢复契约的P1，在后续连接执行器前必须解决。

### 其他边界与测试观察

- **root确有单独验证。** `:75–104` 先读取bounded header和第一条完整Initial/Fork，检查header ordinal0、首sequence0/ordinal1及branch，不会仅凭最新checkpoint声称root存在。
- **不会跳过最新坏checkpoint。** 尾部每条complete记录都先由strict framing及PanngCommit解码；最新checkpoint无效时返回错误，不向更旧checkpoint降级。非换行tail仅计入torn长度，不repair。
- **后缀应用顺序正确。** `:135–154` 逆读结果转正序，校验sequence/predecessor/ordinal连续，并将delta依次apply到候选runtime；无全局RolloutItem静默忽略路径。
- **独立prefix与suffix预算有说明。** header256KiB、首checkpoint16MiB+envelope allowance，尾部32MiB/4096记录；该API不宣称总读取只32MiB。测试含首checkpoint缺失、所有末行截断位置、complete坏行、错误版本/branch/sequence/ordinal、记录数和torn行超限。
- **旧prefix不验证的边界明确。** 测试特意允许最新checkpoint以前的中间无效prefix，符合已选锚点信任模型，不能误标成“错误被静默跳过的suffix bug”。相应报告不得声称全日志完整验证。
- **一个可补的去重检查：** 初始commit被读取后立即drop，后续ids集合只覆盖最新checkpoint及后缀。若某个后方checkpoint复用已知root的commit ID，会被接受。这不要求扫描未知prefix；保留已知root ID及物理位置即可拒绝它在另一位置复用。若当前API刻意仅保证后缀唯一，需将这条限定写明，后续reconcile不能当全日志ID查重结果使用。
- **无历史cut参数是安全的小边界。** 当前只读完整文件，在`:105`取EOF并固定scanner end，不暴露任意父cut自动修复。未来增加frozen-cut API时仍须拒绝落在行内的cut，并区分物理EOFtail。
- **只读仍不保证并发重写下的快照一致。** 当前root读取发生在尾部end捕获前；append-only且prefix不变时成立。后续接入writer修复/截断并发场景应要求稳定文件快照或适当ownership，不能让一个reader混合重写前root与重写后suffix。这是未来mutating reconcile的接入前置条件，本R1没有写操作可保持当前范围。

R1-I1–I3修复后，可接受这个有限只读语义恢复接口；writer commit-ID reconcile、durable re-sync、完整host初始化/投影和生产activation仍为后续工作。

## R1 最新修正复核 disposition

- **R1-I1关闭（静态）。** `panng_recovery.rs:26–43` 新增独立 `PanngReadCut`，RecoveredPanngSuffix不再使用SyncedRecord；文档明确它不是durability ACK或执行授权。`:135–136`只生成读取边界，类型上的混淆已消除。
- **R1-I2关闭（静态）。** `decode_commit`现在接收header thread ID（`:178`），对delta host effects与checkpoint companions中的ThreadSettingsApplied一并要求`Some(header.id)`（`:184–191`）；初始commit和suffix都经过该函数。`checkpoint_and_delta_settings_require_header_thread_ownership`新增null/foreign归属拒绝用例。实际宿主仍应验证被请求thread与返回header.id一致，这不是独立文件reader的执行授权。
- **R1-I3关闭（静态）。** `panng-runtime/src/journal.rs:120–125` 在结构validate层拒绝Fork/ResumeFence含Prepared，不依赖先前日志；对应测试同时覆盖constructor/deserialize拒绝及Compaction/ReplayBound仍可保留Prepared。合法prior→next的apply校验保持，未被此静态规则替代。
- **已知root ID复用检查已补。** `panng_recovery.rs:110`保留root ID；`:130–133`拒绝其他物理位置复用root ID，也继续检查suffix内重复。合法root本身在reverse scan中出现不会误拒。新增`later_checkpoint_cannot_reuse_initial_commit_identity`覆盖此边界；不声称已扫描全部未读prefix的ID。
- **测试状态严格区分。** 主agent报告首轮相关209 tests已通过；本轮检查的是之后新增修正及测试内容，未独立执行，也不把之前209项当作最新修改已通过。

### 本阶段可接受的准确范围

可以接受：有界validated runtime/host codec、明确canonical provenance拒绝策略、native大header兼容预检、foreign marked mode在native变更文件前拒绝、root与最新自包含checkpoint分开验证、有效suffix的branch/thread/ordinal/sequence/ID与runtime转移检查、torn-tail只读描述和独立read cut。

尚未完成且不得据此宣称完成：带stable commit ID的writer提交/reconcile、ACK丢失后重新同步、持锁tail repair、完整host设置/历史/UI发布与恢复接线、生产PanngV1能力、执行epoch授权和真实工具继续执行。对checkpoint以前未读prefix的完整性以及对父fork来源的验证，仍保持报告中已说明的信任范围。

原发现和失败序列留存，便于以后rebase对照；它们已关闭的状态以上述disposition为准。
