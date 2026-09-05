/* ============================================================
 * help.js — 全站帮助按钮与帮助模态框(普通 script 加载,在 manage.js 之后)
 *
 * 纯静态内容模块:不调用任何 Tauri API,不读取用户数据,不修改其他页面。
 * 结构依赖 index.html 末尾追加的:
 * - #help-fab      右下角悬浮帮助按钮(所有页面可见)
 * - #help-modal    帮助模态框(复用 .modal-overlay/.modal-card 体系)
 *   内部 #help-nav(章节导航,由本文件构建)+ #help-content(内容区)
 *
 * 渲染方式:章节内容为自有静态文案(CHAPTERS 数组,模板字符串),
 * 不含任何用户/后端数据,可安全 innerHTML;导航项用 createElement 构建。
 * 交互:按钮打开;「关闭」按钮 / 点击遮罩 / Esc 关闭;章节点击切换并高亮。
 * 样式:style.css 末尾 help 段(遵循 ark token,亮暗主题自动适配)。
 * ============================================================ */
(function () {
  'use strict';

  // ===== 章节数据(num: 导航编号;title: 导航标题;html: 章节正文)=====
  var CHAPTERS = [
    {
      id: 'start',
      num: '',
      title: '快速开始',
      html: `
        <h3>快速开始 START</h3>
        <p>DockerDeploy SSH 是一款桌面端 Docker 部署工具:在本机构建好镜像后,按
        <strong>docker save 导出压缩 → SFTP 上传 → docker load 装载 → docker compose up 启动</strong>
        的流程把应用送上服务器,<strong>全程不依赖任何镜像仓库</strong>(无需 Docker Hub 或私有 Registry)。</p>
        <h4>五页导航(左侧黑色 dock,窄窗口时为顶部横条)</h4>
        <table>
          <thead><tr><th>页码</th><th>页面</th><th>作用</th></tr></thead>
          <tbody>
            <tr><td>01</td><td>环境检测</td><td>检查本机 Docker 环境;通过后解锁 02 / 04 页</td></tr>
            <tr><td>02</td><td>镜像列表</td><td>查看本机镜像,支持搜索,可一键送入部署向导</td></tr>
            <tr><td>03</td><td>服务器管理</td><td>维护服务器(SSH 连接)与部署项目(compose + 文件映射)</td></tr>
            <tr><td>04</td><td>部署向导</td><td>单镜像或整栈(compose)部署到服务器</td></tr>
            <tr><td>05</td><td>远程管理</td><td>经 SSH 管理服务器上的容器 / 镜像 / 卷 / 网络 / 栈 / 实时监控</td></tr>
          </tbody>
        </table>
        <p>02 / 04 页依赖本机 Docker:环境检测未通过时在导航中置灰,点击会提示;
        03 / 05 页只做远程 SSH 操作,始终可以进入。dock 末端的按钮切换亮暗主题。</p>
        <h4>前置要求</h4>
        <ul>
          <li>本机已安装 Docker Desktop 且包含 docker compose 插件(01 页未安装时会给出可复制的安装命令);</li>
          <li>目标服务器可经 SSH 访问:地址、端口、用户名,以及密码或私钥;</li>
          <li>服务器上已安装 Docker(未安装时可用 03 页的「一键安装 Docker」)。</li>
        </ul>
        <h4>推荐首次使用流程</h4>
        <ul>
          <li>01 完成环境检测 → 03 新增服务器并「测试连接」→ 03 新增项目(建议直接导入 compose 文件)→ 04 部署 → 05 远程管理核对运行状态。</li>
        </ul>
        <p>右下角的「?」按钮随时打开本帮助;Esc 或点击遮罩可关闭。</p>`
    },
    {
      id: 'check',
      num: '01',
      title: '环境检测',
      html: `
        <h3>01 环境检测 PREFLIGHT</h3>
        <p>进入页面自动检测一次本机环境;处理完问题后点右上角「重新检测」复检。</p>
        <table>
          <thead><tr><th>检测项</th><th>含义</th><th>未通过时怎么办</th></tr></thead>
          <tbody>
            <tr>
              <td>Docker 已安装</td>
              <td>本机可以执行 docker 命令</td>
              <td>页面给出 <code>winget install Docker.DockerDesktop</code> 与「复制」按钮,以管理员身份在 PowerShell 中执行后重测</td>
            </tr>
            <tr>
              <td>Docker 守护进程运行中</td>
              <td>Docker 引擎已启动、可响应</td>
              <td>点「一键启动 Docker」(最长约 60 秒),或手动启动 Docker Desktop;未就绪时应用每 5 秒自动复检一次,就绪后弹提示</td>
            </tr>
            <tr>
              <td>docker compose 可用</td>
              <td>compose v2 插件可用(整栈部署与 compose 项目依赖它)</td>
              <td>升级 Docker Desktop 至包含 compose 插件的版本后重测</td>
            </tr>
            <tr>
              <td>磁盘空间充足</td>
              <td>临时目录所在盘剩余 ≥ 2 GB(导出镜像需要临时空间)</td>
              <td>运行 Windows 磁盘清理,或执行 <code>docker image prune</code>、<code>docker container prune</code> 删除无用镜像与容器</td>
            </tr>
            <tr>
              <td>本机架构</td>
              <td>信息展示(如 x86_64),不参与判定</td>
              <td>—</td>
            </tr>
          </tbody>
        </table>
        <p>前四项全部通过时顶部显示「环境就绪 READY」并解锁 02 / 04 页;守护进程未运行时,
        compose 项显示「待确认」而不是「未通过」,启动 Docker 后会自动复检。检测命令本身的
        异常信息会显示在顶部错误框中。</p>`
    },
    {
      id: 'images',
      num: '02',
      title: '镜像列表',
      html: `
        <h3>02 镜像列表 IMAGES</h3>
        <p>展示本机 Docker 镜像。首次进入页面自动加载一次,之后靠右上角「刷新」按钮手动更新;
        顶部搜索框按<strong>仓库名或标签</strong>实时过滤(不区分大小写),计数行会显示总数与过滤后数量。</p>
        <ul>
          <li>列:仓库 REPO / TAG / 大小 SIZE / 创建时间 CREATED / 操作;</li>
          <li>仓库名或标签缺失的悬空镜像显示 <code>&lt;none&gt;</code>;<strong>悬空镜像不可部署</strong>,也不会出现在 04 页的镜像下拉里;</li>
          <li>每行「部署」按钮:记住该镜像并跳转到 04 部署向导,自动选中它(用后即清,列表已变化时会提示忽略);</li>
          <li>加载失败时表格区被错误框替换,内含「重试」按钮。</li>
        </ul>`
    },
    {
      id: 'servers',
      num: '03',
      title: '服务器管理',
      html: `
        <h3>03 服务器管理 SERVERS</h3>
        <p>本页维护两类配置:<strong>服务器</strong>(SSH 连接信息)与<strong>部署项目</strong>(compose 与文件映射)。
        右上角「新增服务器」「新增项目」打开编辑表单;本机环境检测未通过时顶部有黄色提示条,但配置编辑不受影响。</p>
        <h4>服务器表单逐字段</h4>
        <table>
          <thead><tr><th>字段</th><th>说明</th></tr></thead>
          <tbody>
            <tr><td>名称</td><td>仅本地显示的标识,如「生产服务器」</td></tr>
            <tr><td>主机(IP 或域名)</td><td>服务器的 IP 或域名,如 192.168.1.100</td></tr>
            <tr><td>端口</td><td>SSH 端口,1 - 65535,默认 22</td></tr>
            <tr><td>用户名</td><td>SSH 登录用户,如 root</td></tr>
            <tr>
              <td>认证方式</td>
              <td><strong>私钥</strong>:填写本机私钥文件的绝对路径(可点「浏览」选择);<strong>密码</strong>:填写登录密码</td>
            </tr>
            <tr>
              <td>登录密码</td>
              <td>保存时经操作系统加密(DPAPI)后存储,界面不回显明文;编辑时<strong>留空表示沿用已保存密码</strong>,输入新密码即覆盖;切回私钥认证时旧密文保留,便于再切回</td>
            </tr>
            <tr><td>远程部署目录</td><td>服务器上的部署根目录,如 /opt/myapp;compose 上传、文件映射、栈发现都以它为基准;目录不存在时检测结果区会出现「创建远程目录」按钮</td></tr>
          </tbody>
        </table>
        <ul>
          <li>必填:名称、主机、用户名、远程目录;密码认证必须填密码,私钥认证必须填私钥路径;</li>
          <li>保存成功后自动发起一次「测试连接」,结果直接显示在服务器卡片上。</li>
        </ul>
        <h4>服务器卡片上的按钮</h4>
        <table>
          <thead><tr><th>按钮</th><th>点击后发生什么</th></tr></thead>
          <tbody>
            <tr><td>测试连接 / 环境检测</td><td>SSH 连上服务器并检测 Docker、Compose、gzip、远程目录、磁盘剩余空间,结果以徽章显示在卡片内;有问题时逐行列出错误明细。两个按钮能力等价</td></tr>
            <tr><td>一键安装 Docker</td><td>仅 Docker 未通过时出现;内联确认条确认后,经 SSH 执行官方安装脚本(可能需要数分钟),过程输出写入页面底部「运行日志」,结束后自动复检刷新徽章</td></tr>
            <tr><td>清理优化</td><td>内联确认条确认后,清理服务器上的悬空镜像与已退出容器(超时 300 秒),输出见「运行日志」</td></tr>
            <tr><td>编辑</td><td>打开表单修改当前服务器</td></tr>
            <tr><td>删除</td><td>按钮先变「确认删除?」,3 秒内再点一次才真正删除(防误触);项目删除同理</td></tr>
          </tbody>
        </table>
        <h4>项目表单逐字段(部署项目 = 服务器 + compose + 文件映射)</h4>
        <table>
          <thead><tr><th>字段</th><th>说明</th></tr></thead>
          <tbody>
            <tr><td>导入 compose 文件</td><td>仅新增时可选:选择本地 docker-compose.yml / .yaml,保存时复制进应用配置目录,并按解析结果自动生成每个服务的传输分类;「解析预览」展示服务 / 镜像 / 匹配徽章 / 默认分类,解析存在阻断问题时禁止保存。填写后下方「compose 相对路径」置灰</td></tr>
            <tr><td>名称</td><td>项目标识;导入路径变化时自动预填为文件名(手改过则不覆盖)</td></tr>
            <tr><td>镜像过滤关键字</td><td>部署时按它匹配本地镜像仓库名;留空匹配全部镜像</td></tr>
            <tr><td>compose 文件相对路径</td><td>未走导入时手工填写,相对远程部署目录,如 docker-compose.yml</td></tr>
            <tr><td>文件映射(本地 → 服务器)</td><td>每行:本地绝对路径 + 服务器相对路径,勾选「目录」表示映射整个目录;本地格可「浏览」选择;两格都留空的行保存时被忽略。部署时这些文件会同步到服务器</td></tr>
            <tr><td>健康检查</td><td>部署后轮询容器状态的最长等待秒数,0 为关闭(上限 86400)</td></tr>
            <tr><td>pre-deploy / post-deploy 命令</td><td>部署前 / 部署后在项目目录执行的 shell 命令;下方预设 chips 点击插入后可再修改;留空不执行;<strong>pre 失败将中止部署</strong></td></tr>
            <tr><td>完成通知 webhook</td><td>http(s) 地址,部署结束后 POST JSON 结果;留空关闭</td></tr>
          </tbody>
        </table>
        <p>页面底部「运行日志」折叠面板:安装 Docker、清理优化等远端命令的输出(最多保留 500 行),相关操作开始时会自动展开。</p>`
    },
    {
      id: 'deploy',
      num: '04',
      title: '部署向导',
      html: `
        <h3>04 部署向导 DEPLOY</h3>
        <p>页首两个模式 tab:<strong>单镜像</strong>与<strong>整栈部署(compose)</strong>;每次进入页面都会重新加载镜像与配置。
        部署 / 预检进行中,模式切换与所有下拉、按钮都会锁定。</p>
        <h4>单镜像部署</h4>
        <ul>
          <li>依次选择:本地镜像 + 目标服务器 + 部署项目(三项都必选);</li>
          <li>「标签选项」勾选<strong>打日期标签(推荐)</strong>:为镜像追加「仓库名:日期时间」标签,并在服务器上同步 compose 引用的原标签——更新生效,且可按日期回滚;</li>
          <li>点「开始部署」后先对服务器做<strong>环境预检</strong>(徽章列出 Docker / Compose / gzip / 远程目录 / 磁盘),未通过时显示错误并给「跳转服务器管理」按钮;通过后依次执行 5 个节点:</li>
        </ul>
        <table>
          <thead><tr><th>节点</th><th>内容</th></tr></thead>
          <tbody>
            <tr><td>01 打标签 TAG</td><td>为本机镜像打上部署标签</td></tr>
            <tr><td>02 导出压缩 PACK</td><td>docker save 导出并压缩(需要临时磁盘空间)</td></tr>
            <tr><td>03 上传镜像 UPLOAD</td><td>SFTP 上传到服务器</td></tr>
            <tr><td>04 同步文件 SYNC</td><td>上传 compose 并同步项目的文件映射</td></tr>
            <tr><td>05 服务器部署 APPLY</td><td>服务器上 docker load 装载并 compose up 启动</td></tr>
          </tbody>
        </table>
        <h4>整栈部署(compose)</h4>
        <ul>
          <li>选择项目后<strong>自动解析</strong> compose 并生成服务分类表(也可点「解析服务」手动重解析);</li>
          <li>每行显示:服务、镜像、匹配徽章(已匹配 / 标签不一致 / 本地不存在)与「传输方式」按钮——<strong>本地传输 ↔ 服务器拉取</strong>逐服务切换;compose 未设 image 字段的服务锁定为本地传输;</li>
          <li>「部署预览」:独立的 dry-run,对比远端实际状态给出 重建 / 新建 / 不变 / 拉取 / 缺失 徽章;不自动触发,也不影响开始部署;切换项目或服务器后旧预览失效;</li>
          <li>「保存为默认分类」:把当前每个服务的传输方式写回项目配置;</li>
          <li>解析存在问题(红框提示)会<strong>阻断</strong>开始部署;执行节点为 6 个:分类确认 → 打包 → 上传 → 装载 → 拉取 → 启动。</li>
        </ul>
        <h4>进度、日志与历史</h4>
        <ul>
          <li>部署中「开始部署」禁用、「取消部署」可用——取消请求在当前步骤结束后生效;</li>
          <li>底部日志区实时输出(带时间戳,上限 2000 行,自动滚底,右下角行计数;向上翻看历史时不强制拉底);</li>
          <li>「部署历史」折叠面板:记录时间 / 模式 / 项目与服务器 / 镜像 / 结果 / 耗时(最新 50 条),进入页面与每次部署结束后自动刷新。</li>
        </ul>
        <div class="help-warn">开始部署前请确认 01 页环境检测已通过、目标服务器预检全部通过;导出压缩需要临时磁盘空间,空间不足会在检测与预检中被拦截。</div>`
    },
    {
      id: 'manage-overview',
      num: '05',
      title: '远程管理·总览',
      html: `
        <h3>05 远程管理·总览 REMOTE</h3>
        <p>本页通过 SSH 直接管理远程服务器上的 Docker,<strong>不依赖本机 Docker</strong>。
        页面结构自上而下:服务器栏 → 概览面板 → 六个 Tab(容器 / 镜像 / 卷 / 网络 / 栈 / 监控)。</p>
        <table>
          <thead><tr><th>控件</th><th>说明</th></tr></thead>
          <tbody>
            <tr><td>服务器下拉</td><td>列出 03 页配置的服务器(名称 (主机)),默认选中第一台并自动加载;切换服务器会清空展开状态与缓存、停掉旧服务器上的监控与终端会话,然后刷新</td></tr>
            <tr><td>状态徽章</td><td>未连接 / 连接中… / 已连接 / 连接失败,反映最近一次概览请求结果</td></tr>
            <tr><td>刷新按钮</td><td>手动刷新概览 + 当前 Tab 的列表</td></tr>
            <tr><td>自动刷新</td><td>开启后按间隔自动刷新概览与当前 Tab;间隔可选 5 / 10 / 30 / 60 秒或「自定义…」(<strong>3 - 300 秒</strong>,弹出输入框);开关与间隔会记忆在本地,下次打开恢复</td></tr>
            <tr><td>概览面板</td><td>Docker 版本 / 操作系统 / 内核 / 架构 / 容器(运行/暂停/停止/总数)/ 镜像数 / 磁盘占用</td></tr>
          </tbody>
        </table>
        <ul>
          <li>切换 Tab 时自动加载该 Tab 的数据;自动刷新在「上一轮请求还没返回」或「有操作进行中」时跳过该轮(防重入);</li>
          <li>离开 05 页会自动停止自动刷新、监控与终端会话;切走监控 Tab 也会停止监控。</li>
        </ul>`
    },
    {
      id: 'manage-containers',
      num: '05',
      title: '容器',
      html: `
        <h3>05 容器 CONTAINERS</h3>
        <p>列:状态 / 名称 / 镜像 / 端口 / 创建时间 / 操作。<strong>点击容器名称</strong>可展开(或收起)详情面板,
        显示 inspect 关键信息:状态、重启次数、启动时间、镜像、IP 地址、挂载;端口超过 2 个时折叠显示,
        点击端口格展开完整列表。状态徽章:运行中 / 已暂停 / 重启中 / 已停止 / 已创建等。</p>
        <h4>每个按钮与点击后的行为</h4>
        <table>
          <thead><tr><th>按钮</th><th>点击后发生什么</th></tr></thead>
          <tbody>
            <tr><td>启动</td><td>已停止的容器显示;点击立即执行(无确认),成功后 toast 提示并自动刷新列表与概览</td></tr>
            <tr><td>停止</td><td>运行中的容器显示;点击立即执行,成功后自动刷新列表与概览</td></tr>
            <tr><td>重启</td><td>运行中的容器显示;点击立即执行,成功后自动刷新列表与概览</td></tr>
            <tr><td>日志</td><td>弹出模态显示该容器最近日志:行数可选 100 / 500 / 1000 / 全部,切换立即重新拉取;右上「复制日志」一键复制当前内容</td></tr>
            <tr><td>终端</td><td>仅运行中容器显示。打开交互式 shell:<strong>默认 bash,可切换 sh</strong>(切换会重开会话);底部输入框输入命令回车执行,↑ / ↓ 翻本会话命令历史;输出保留最近 1000 行。<strong>关闭终端按钮或关闭弹窗即断开会话</strong>;同一时间只允许一个终端会话。容器内操作与 SSH 登进该容器执行等效,删除类命令请谨慎</td></tr>
            <tr><td>删除</td><td>弹出二次确认;确认后删除容器。运行中的容器会被<strong>强制停止并删除</strong>(等效 docker rm -f),容器内未持久化到卷的数据一并丢失</td></tr>
          </tbody>
        </table>`
    },
    {
      id: 'manage-images',
      num: '05',
      title: '镜像',
      html: `
        <h3>05 镜像 IMAGES</h3>
        <p>列:仓库 / 标签 / ID / 大小 / 创建时间 / 操作。顶部为「拉取镜像」输入栏。</p>
        <h4>每个操作与点击后的行为</h4>
        <table>
          <thead><tr><th>操作</th><th>说明</th></tr></thead>
          <tbody>
            <tr>
              <td>拉取镜像</td>
              <td>输入「仓库:标签」(如 <code>nginx:latest</code>)后点「拉取镜像」或直接回车;服务器从镜像仓库拉取,<strong>大镜像可能需要数分钟</strong>,期间按钮禁用、自动刷新暂停;完成后自动刷新列表与概览。仓库名缺失的服务器端按默认仓库处理,写清全名更稳妥</td>
            </tr>
            <tr>
              <td>打标签</td>
              <td>弹窗中源镜像只读,输入新标签(格式 <strong>仓库名:标签</strong>,如 myrepo/myapp:v1);只在本服务器上为同一镜像新增一个标签,<strong>不会推送到任何仓库</strong>;成功后刷新列表</td>
            </tr>
            <tr>
              <td>删除</td>
              <td>弹出二次确认,可勾选「强制删除(-f)」;<strong>镜像被容器引用时普通删除会失败</strong>——先删除相关容器,或勾选强制删除;成功后刷新列表与概览</td>
            </tr>
          </tbody>
        </table>`
    },
    {
      id: 'manage-volumes-networks',
      num: '05',
      title: '卷与网络',
      html: `
        <h3>05 卷 VOLUMES / 网络 NETWORKS</h3>
        <h4>卷</h4>
        <ul>
          <li><strong>创建卷</strong>:弹窗输入卷名称(必填)与驱动(<strong>留空默认 local</strong>);成功后刷新列表;</li>
          <li><strong>查看</strong>:弹窗显示该卷 inspect 的原始 JSON;</li>
          <li><strong>删除</strong>:二次确认;<strong>正被容器使用的卷会删除失败</strong>,先停止并删除使用它的容器。</li>
        </ul>
        <h4>网络</h4>
        <ul>
          <li><strong>创建网络</strong>:弹窗输入网络名称(必填)与驱动(<strong>留空默认 bridge</strong>);</li>
          <li><strong>查看</strong>:inspect 原始 JSON;</li>
          <li><strong>连接容器 / 断开容器</strong>:弹窗输入<strong>容器名或容器 ID</strong>;连接要求容器处于<strong>运行中</strong>;成功后刷新列表(「已连接容器」列会变化);</li>
          <li><strong>删除</strong>:二次确认;有容器连接在该网络上时删除失败;<strong>bridge / host / none 为 Docker 内置网络,不提供删除按钮</strong>。</li>
        </ul>
        <p>列:卷=名称 / 驱动 / 挂载点 / 创建时间(Docker 25+ 才提供,旧版显示 —);网络=名称 / 驱动 / 范围 / 已连接容器数。</p>`
    },
    {
      id: 'manage-stacks',
      num: '05',
      title: 'Compose 栈',
      html: `
        <h3>05 Compose 栈 STACKS</h3>
        <p>栈由后端<strong>自动发现</strong>:在服务器的远程目录内向下最多 2 层查找
        <code>docker-compose.yml</code> / <code>docker-compose.yaml</code> / <code>compose.yml</code> / <code>compose.yaml</code>。
        没看到栈时,先确认 compose 文件是否位于远程目录 2 层以内,再点「刷新栈」重新扫描。</p>
        <h4>每个按钮与点击后的行为</h4>
        <table>
          <thead><tr><th>按钮</th><th>点击后发生什么</th></tr></thead>
          <tbody>
            <tr><td>刷新栈</td><td>重新扫描远程目录,重建栈列表</td></tr>
            <tr><td>启动</td><td>二次确认后执行 <code>docker compose up -d</code>:后台创建并启动该栈的全部服务(含拉取缺失镜像),<strong>最多等待 2 分钟</strong>;成功后刷新栈列表与概览</td></tr>
            <tr><td>停止</td><td>二次确认后执行 <code>docker compose down</code>:停止并移除该栈的容器;<strong>数据卷默认保留</strong>,但容器本身会被移除</td></tr>
            <tr><td>服务状态</td><td>弹窗表格显示 compose ps 的结果:服务名 + 运行状态(运行中 / 已退出 / …)</td></tr>
            <tr><td>日志</td><td>弹窗显示 compose logs 输出,行数可选 100 / 500 / 1000 / 全部</td></tr>
          </tbody>
        </table>
        <div class="help-warn">「停止(down)」会移除容器(卷保留);若只想暂停服务,请改用容器 Tab 的「停止」,或在 compose 中管理 replicas。</div>`
    },
    {
      id: 'manage-monitor',
      num: '05',
      title: '实时监控',
      html: `
        <h3>05 实时监控 MONITOR</h3>
        <p>点「开始监控」按所选间隔(<strong>1 / 2 / 5 / 10 秒</strong>,运行中不可修改)轮询 docker stats,
        整表刷新;「停止监控」、切走本 Tab 或离开 05 页都会自动停止。徽章显示「监控中 / 已停止」。</p>
        <h4>各列含义</h4>
        <table>
          <thead><tr><th>列</th><th>含义</th></tr></thead>
          <tbody>
            <tr><td>容器名 NAME</td><td>容器名称(缺失时显示容器 ID)</td></tr>
            <tr><td>CPU%</td><td>容器 CPU 占用(相对单核 100%)</td></tr>
            <tr><td>内存占用 MEM</td><td>实际用量 / 限额</td></tr>
            <tr><td>内存% MEM%</td><td>内存用量占限额的百分比</td></tr>
            <tr><td>网络 IO NET IO</td><td>累计网络收 / 发流量</td></tr>
            <tr><td>块 IO BLOCK IO</td><td>累计磁盘读 / 写</td></tr>
            <tr><td>PID</td><td>容器内进程数</td></tr>
          </tbody>
        </table>
        <ul>
          <li>CPU% 颜色阈值:<strong>&gt;80% 标红,50 - 80% 标黄,&lt;50% 正常</strong>;</li>
          <li><strong>连续 3 轮连接失败</strong>,或 SSH 用户无 Docker 权限(permission denied … docker.sock)时,监控<strong>自动停止</strong>并在面板顶部给出原因;单轮失败仅提示,下一轮自动重试;</li>
          <li>监控只在停留本 Tab 时运行;切服务器会先停掉旧服务器上的监控。</li>
        </ul>
        <p>提示:旧版 Docker 的 docker stats 输出格式不同,可能出现列显示异常或为空,升级 Docker 可解决。</p>`
    },
    {
      id: 'faq',
      num: 'FAQ',
      title: '常见问题',
      html: `
        <h3>常见问题 FAQ</h3>
        <h4>远程操作提示 permission denied … /var/run/docker.sock</h4>
        <p>SSH 用户无权访问 Docker 守护进程。把用户加入 docker 组后<strong>重新 SSH 登录</strong>:
        <code>sudo usermod -aG docker 用户名</code>;或直接使用 root 登录。</p>
        <h4>连不上服务器</h4>
        <ul>
          <li>检查主机、端口是否正确,云服务器安全组 / 防火墙是否放行 SSH 端口;</li>
          <li>认证方式与凭据是否匹配:密码认证确认密码,私钥认证确认<strong>本机</strong>私钥文件路径与服务器上的公钥配对;</li>
          <li>确认服务器 sshd 允许该用户登录;回 03 页用「测试连接」复现具体错误信息。</li>
        </ul>
        <h4>监控列显示异常或为空</h4>
        <p>旧版 Docker 的 docker stats 不支持 JSON 输出,监控可能出现列错位或为空;升级服务器上的 Docker 可解决。</p>
        <h4>删除失败的一般原因</h4>
        <ul>
          <li>资源正被引用:镜像被容器引用(先删容器,或镜像删除时勾选强制)、卷正被容器使用、网络上有容器连接;</li>
          <li>内置网络 bridge / host / none 不可删除(界面上也不提供删除按钮);</li>
          <li>操作结果都会以 toast 提示,失败原因直接跟在提示文本后。</li>
        </ul>
        <h4>自动刷新为什么不跑?</h4>
        <ul>
          <li>已离开 05 页——自动刷新、监控、终端都会在离开时停止;</li>
          <li>上一轮刷新请求尚未返回,或某个操作(启停容器、拉取镜像、栈操作等)正在进行,该轮会被跳过,下轮恢复;</li>
          <li>自动刷新开关未打开,或间隔设置异常(有效范围 3 - 300 秒)。</li>
        </ul>
        <h4>部署页镜像下拉为空?</h4>
        <p>仓库名或标签为 <code>&lt;none&gt;</code> 的悬空镜像不会出现在部署向导;先构建或重新打标签,
        并确认 01 页环境检测已通过。</p>`
    }
  ];

  // ===== 状态与 DOM =====
  var navBuilt = false; // 章节导航只构建一次
  var current = -1;     // 当前章节索引

  function $(id) { return document.getElementById(id); }

  // ===== 章节导航(点击切换;当前项高亮信号青)=====
  function buildNav() {
    var nav = $('help-nav');
    if (!nav) return;
    nav.textContent = '';
    CHAPTERS.forEach(function (ch, i) {
      var btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'help-nav-item';
      btn.setAttribute('data-index', String(i));
      var num = document.createElement('span');
      num.className = 'help-nav-num';
      num.textContent = ch.num || '··';
      btn.appendChild(num);
      btn.appendChild(document.createTextNode(ch.title));
      btn.addEventListener('click', function () { selectChapter(i); });
      nav.appendChild(btn);
    });
  }

  function selectChapter(i) {
    if (i < 0 || i >= CHAPTERS.length) return;
    current = i;
    var nav = $('help-nav');
    if (nav) {
      var items = nav.querySelectorAll('.help-nav-item');
      for (var k = 0; k < items.length; k++) {
        items[k].classList.toggle('active',
          Number(items[k].getAttribute('data-index')) === i);
      }
    }
    var content = $('help-content');
    if (content) {
      content.innerHTML = CHAPTERS[i].html; // 静态自有文案,无用户数据,可安全 innerHTML
      content.scrollTop = 0;
    }
  }

  // ===== 打开 / 关闭 =====
  function openHelp() {
    var modal = $('help-modal');
    if (!modal) return;
    if (!navBuilt) {
      buildNav();
      selectChapter(0);
      navBuilt = true;
    }
    modal.classList.remove('hidden');
    var closeBtn = $('help-modal-close');
    if (closeBtn) closeBtn.focus();
  }

  function closeHelp() {
    var modal = $('help-modal');
    if (modal) modal.classList.add('hidden');
  }

  // ===== 初始化(DOMContentLoaded 后绑定)=====
  function init() {
    var fab = $('help-fab');
    if (fab) fab.addEventListener('click', openHelp);

    var closeBtn = $('help-modal-close');
    if (closeBtn) closeBtn.addEventListener('click', closeHelp);

    var overlay = $('help-modal');
    if (overlay) {
      // 点击遮罩空白处关闭(点在卡片内部不关)
      overlay.addEventListener('click', function (e) {
        if (e.target === overlay) closeHelp();
      });
      // Esc 关闭(仅帮助模态可见时,避免误伤其他模态)
      document.addEventListener('keydown', function (e) {
        if (e.key === 'Escape' && !overlay.classList.contains('hidden')) {
          closeHelp();
        }
      });
    }
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
})();
