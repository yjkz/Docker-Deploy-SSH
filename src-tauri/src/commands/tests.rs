    use super::*;
    use crate::config::{AuthConfig, TransferMode};

    // ===== 清理分析纯函数(第三批)=====

    #[test]
    fn test_parse_cleanup_ndjson_skips_non_json() {
        // stderr 混入(如 "WARNING: No swap limit support")不应让整节失败;
        // 以 `{` 开头但结构损坏的行单独报"解析失败"
        let out = "WARNING: No swap limit support\n{\"ID\":\"abc\",\"Repository\":\"x\"}\n\n{ broken json\n";
        let (items, warnings) = parse_cleanup_ndjson(out);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["ID"], "abc");
        // 提示行进 warnings;损坏的 JSON 行报解析失败
        assert!(warnings.iter().any(|w| w.contains("No swap limit")));
        assert!(warnings.iter().any(|w| w.contains("解析失败")));
    }

    #[test]
    fn test_parse_cleanup_ndjson_warning_cap() {
        // 提示行最多收集 3 条,防刷屏
        let out = "a\nb\nc\nd\ne\n";
        let (items, warnings) = parse_cleanup_ndjson(out);
        assert!(items.is_empty());
        assert_eq!(warnings.len(), 3);
    }

    #[test]
    fn test_image_repo_of() {
        assert_eq!(image_repo_of("myapp:latest"), "myapp");
        assert_eq!(image_repo_of("myapp"), "myapp");
        // registry:port/name 里的冒号不是 tag 分隔符
        assert_eq!(image_repo_of("registry:5000/app"), "registry:5000/app");
        assert_eq!(image_repo_of("registry:5000/app:v1"), "registry:5000/app");
        assert_eq!(image_repo_of("app@sha256:deadbeef"), "app");
        assert_eq!(image_repo_of(""), "");
    }

    #[test]
    fn test_is_date_tag() {
        assert!(is_date_tag("20260905-101010"));
        assert!(!is_date_tag("latest"));
        assert!(!is_date_tag("20260905"));
        assert!(!is_date_tag("2026090-1010100"));
        assert!(!is_date_tag("2026090x-101010"));
    }

    #[test]
    fn test_compose_image_repos() {
        let yaml = "\
services:
  web:
    image: myapp:latest
  db:
    image: registry:5000/postgres:16
  worker:
    image: myapp:latest
";
        let repos = compose_image_repos(yaml);
        assert!(repos.contains(&"myapp".to_string()));
        assert!(repos.contains(&"registry:5000/postgres".to_string()));
        // 去重:myapp 只出现一次
        assert_eq!(repos.iter().filter(|r| *r == "myapp").count(), 1);
        // 解析失败 → 空(调用方据此跳过标签清理,不误删)
        assert!(compose_image_repos("}{ not yaml").is_empty());
        assert!(compose_image_repos("services: {}").is_empty());
    }

    #[test]
    fn test_split_compose_dump() {
        let out = "==COMPOSE:/home/a/docker-compose.yml\nservices:\n  web:\n    image: x\n\n==COMPOSE:/home/b/docker-compose.yml\nservices:\n  db:\n    image: y\n\n";
        let parts = split_compose_dump(out);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].0, "/home/a/docker-compose.yml");
        assert!(parts[0].1.contains("image: x"));
        assert_eq!(parts[1].0, "/home/b/docker-compose.yml");
        assert!(parts[1].1.contains("image: y"));
    }

    #[test]
    fn test_project_dir_of_release() {
        assert_eq!(
            project_dir_of_release("/home/henghao/zetok/releases/20260905-101010"),
            "/home/henghao/zetok"
        );
        assert_eq!(
            project_dir_of_release("/home/henghao/zetok/releases/20260905-101010/"),
            "/home/henghao/zetok"
        );
        assert_eq!(project_dir_of_release("/tmp/x"), "");
    }

    #[test]
    fn test_parse_du_output() {
        // GNU du:大小与路径以制表符分隔
        let rows = parse_du_output("1.2G\t/home/a/proj\n340M\t/home/a/other\n");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], ("/home/a/proj".to_string(), "1.2G".to_string()));
        assert_eq!(rows[1], ("/home/a/other".to_string(), "340M".to_string()));
        // 退化为空格分隔
        let rows2 = parse_du_output("12K /home/a/b");
        assert_eq!(rows2[0], ("/home/a/b".to_string(), "12K".to_string()));
        assert!(parse_du_output("").is_empty());
    }

    #[test]
    fn test_abs_path_lines() {
        let out = "/home/a/docker-compose.yml\nrunning\n… (共 12 行)\n/home/b/compose.yml\n";
        let lines = abs_path_lines(out);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "/home/a/docker-compose.yml");
    }

    #[test]
    fn test_cleanup_cmd_builders_quote() {
        // 含空格与单引号的路径必须被安全包裹
        let dirs = vec!["/home/a b/releases/20260905-101010".to_string()];
        assert_eq!(
            rm_release_dirs_cmd(&dirs),
            "rm -rf '/home/a b/releases/20260905-101010'"
        );
        let ids = vec!["sha256:abc".to_string()];
        assert_eq!(rmi_ids_cmd(&ids), "docker rmi 'sha256:abc'");
        assert_eq!(
            rm_volume_names_cmd(&["vol1".to_string()]),
            "docker volume rm 'vol1'"
        );
    }

    #[test]
    fn test_cleanup_scan_compose_cmd_excludes_archives() {
        let cmd = cleanup_scan_compose_cmd("/home/henghao");
        assert!(cmd.contains("-maxdepth 4"));
        assert!(cmd.contains("'*/releases/*'"));
        assert!(cmd.contains("'*/.git/*'"));
        assert!(cmd.contains("/home/henghao"));
    }

    // ---- 第七批:回滚明细批量读取 + 版本说明 ----

    #[test]
    fn test_releases_scan_cmd_markers_and_flags() {
        let candidates = vec!["/app/docker-compose.yml".to_string()];
        let with_images = releases_scan_cmd("/app", 50, &candidates, true);
        // find 循环 + 截断 + 三类归档段 + compose 段 + 镜像段
        assert!(with_images.contains("find '/app/releases' -mindepth 1 -maxdepth 1 -type d"));
        assert!(with_images.contains("sort -r | head -n 50"));
        assert!(with_images.contains("while IFS= read -r d"));
        assert!(with_images.contains("==RELEASE:%s"));
        assert!(with_images.contains("==MANIFEST:%s"));
        assert!(with_images.contains("==NOTES:%s"));
        assert!(with_images.contains("==COMPOSE:%s"));
        assert!(with_images.contains("==IMAGES"));
        assert!(with_images.contains("'/app/docker-compose.yml'"));
        // 归档内路径由远端循环变量拼接($d),不再逐归档展开字面路径
        assert!(with_images.contains("cat \"$d/manifest.json\""));
        assert!(with_images.contains("cat \"$d/release-notes.json\""));
        // include_images=false 时不带镜像段(list_releases 用)
        let no_images = releases_scan_cmd("/app", 100, &[], false);
        assert!(!no_images.contains("==IMAGES"));
        assert!(no_images.contains("head -n 100"));
    }

    #[test]
    fn test_parse_releases_dump_sections() {
        let candidates = vec!["/app/docker-compose.yml".to_string()];
        // 远端输出按归档分组:每个归档的 RELEASE/MANIFEST/NOTES 相邻
        let out = "==RELEASE:20260905-101010\n\
                   app.tar.gz\n\
                   manifest.json\n\
                   ==MANIFEST:20260905-101010\n\
                   {\"project\":\"app\",\"images\":[]}\n\
                   ==RELEASE:20260901-090000\n\
                   ==NOTES:20260901-090000\n\
                   {\"title\":\"v1\",\"body\":\"fix\",\"updatedAt\":\"t\"}\n\
                   ==COMPOSE:/app/docker-compose.yml\n\
                   services:\n  web:\n    image: app:latest\n\
                   ==IMAGES\n\
                   {\"Repository\":\"app\",\"Tag\":\"20260901-090000\"}\n";
        let dump = parse_releases_dump(out, &candidates);
        assert_eq!(dump.entries.len(), 2);
        assert_eq!(dump.entries[0].ts, "20260905-101010");
        assert_eq!(dump.entries[0].files, vec!["app.tar.gz", "manifest.json"]);
        assert!(dump.entries[0].manifest.contains("\"project\":\"app\""));
        assert!(dump.entries[0].notes.is_empty());
        // 第二个归档只有 NOTES 段(files/manifest 为空,不错位)
        assert_eq!(dump.entries[1].ts, "20260901-090000");
        assert!(dump.entries[1].files.is_empty());
        assert!(dump.entries[1].notes.contains("\"title\":\"v1\""));
        assert!(dump.compose_texts[0].contains("image: app:latest"));
        assert!(dump.images.contains("\"Repository\":\"app\""));
    }

    #[test]
    fn test_parse_releases_dump_empty_and_unknown() {
        let dump = parse_releases_dump("", &[]);
        assert!(dump.entries.is_empty());
        assert!(dump.images.is_empty());
        // 标记即条目:==RELEASE 后的清单行归入该条目,文件清单忽略空行;
        // ==IMAGES 段独立收集
        let dump = parse_releases_dump(
            "==RELEASE:99999999-999999\nstray.tar.gz\n\n==IMAGES\n{x}\n",
            &[],
        );
        assert_eq!(dump.entries.len(), 1);
        assert_eq!(dump.entries[0].ts, "99999999-999999");
        assert_eq!(dump.entries[0].files, vec!["stray.tar.gz"]);
        assert!(dump.images.contains("{x}"));
    }

    #[test]
    fn test_rollback_scan_cmd_sections() {
        let cmd = rollback_scan_cmd("/home/henghao");
        assert!(cmd.starts_with("[ -d '/home/henghao' ]"));
        assert!(cmd.contains("==ROOTEXISTS:1"));
        assert!(cmd.contains("==COMPOSEFILES"));
        assert!(cmd.contains("==RELEASES"));
        assert!(cmd.contains("==PSALL"));
        assert!(cmd.contains("docker ps -a --format '{{json .}}'"));
        // compose find 复用清理分析的拼装(深度 4,排除 releases/.git)
        assert!(cmd.contains("-maxdepth 4"));
        assert!(cmd.contains("'*/releases/*'"));
    }

    #[test]
    fn test_parse_rollback_scan_sections() {
        let out = "==ROOTEXISTS:1\n\
                   ==COMPOSEFILES\n\
                   /home/x/app/docker-compose.yml\n\
                   ==RELEASES\n\
                   /home/x/app/releases/20260905-101010\n\
                   ==PSALL\n\
                   {\"Names\":\"app\",\"Status\":\"Up 1 hour\",\"Labels\":{\"com.docker.compose.project.working_dir\":\"/home/x/app\"}}\n\
                   not-json\n";
        let scan = parse_rollback_scan(out);
        assert!(scan.root_exists);
        assert_eq!(scan.compose_paths, vec!["/home/x/app/docker-compose.yml"]);
        assert_eq!(
            scan.release_paths,
            vec!["/home/x/app/releases/20260905-101010"]
        );
        assert_eq!(scan.ps_items.len(), 1); // 坏行跳过
        assert_eq!(compose_working_dirs(&scan.ps_items), vec!["/home/x/app"]);
    }

    #[test]
    fn test_parse_rollback_scan_root_missing() {
        let scan = parse_rollback_scan("==ROOTEXISTS:0\n");
        assert!(!scan.root_exists);
    }

    #[test]
    fn test_compose_working_dirs_dedupe_and_skip_empty() {
        let items: Vec<serde_json::Value> = vec![
            serde_json::json!({"Labels": {"com.docker.compose.project.working_dir": "/a"}}),
            serde_json::json!({"Labels": {"com.docker.compose.project.working_dir": "/a"}}),
            serde_json::json!({"Names": "no-labels"}),
        ];
        assert_eq!(compose_working_dirs(&items), vec!["/a"]);
    }

    #[test]
    fn test_release_notes_write_cmd_atomic() {
        let cmd = release_notes_write_cmd("/app/releases/x/release-notes.json", "QUJD");
        // 与 .env 原子写同构:b64 进、tmp 带引号外 $$ 后缀、mv 原子替换到单引号包裹的目标
        assert!(cmd.starts_with("echo QUJD | base64 -d > "));
        assert!(cmd.contains("'.ddtmp.'$$"));
        assert!(cmd.contains("&& mv '"));
        assert!(cmd.ends_with(" '/app/releases/x/release-notes.json'"));
    }

    #[test]
    fn test_parse_release_notes_invalid() {
        assert!(parse_release_notes("").is_none());
        assert!(parse_release_notes("not json").is_none());
        assert!(parse_release_notes("{\"title\":\"缺 body\"}").is_none());
        let n = parse_release_notes(
            "{\"title\":\"v1\",\"body\":\"desc\",\"updatedAt\":\"2026-09-12T00:00:00Z\"}",
        );
        assert_eq!(n.unwrap().body, "desc");
    }

    #[test]
    fn test_cleanup_sections_has_any() {
        let empty = CleanupSections {
            images: true,
            containers: false,
            volumes: false,
            builder: false,
            image_ids: vec![],
            container_ids: vec![],
            volume_names: vec![],
            projects: vec![],
        };
        // 勾了节但没有任何目标 → 不可执行(防"勾了却没东西可删"的误报成功)
        assert!(!empty.has_any());
        let with_target = CleanupSections {
            images: true,
            containers: false,
            volumes: false,
            builder: false,
            image_ids: vec!["sha256:x".into()],
            container_ids: vec![],
            volume_names: vec![],
            projects: vec![],
        };
        assert!(with_target.has_any());
        let builder_only = CleanupSections {
            images: false,
            containers: false,
            volumes: false,
            builder: true,
            image_ids: vec![],
            container_ids: vec![],
            volume_names: vec![],
            projects: vec![],
        };
        assert!(builder_only.has_any());
    }

    #[test]
    fn test_effective_remote_dir_fallback_semantics() {
        // 第四批:项目级目录优先;未配置(旧配置)必须完全回落到服务器目录
        let server = ServerConfig {
            id: "s1".into(),
            name: "美国".into(),
            host: "1.2.3.4".into(),
            port: 22,
            username: "root".into(),
            auth: AuthConfig {
                auth_type: crate::config::AuthType::Password,
                key_path: None,
                password_enc: None,
                key_pass_enc: None,
            },
            remote_dir: "/home/henghao".into(),
            host_key_sha256: None,
        };
        let mk_project = |dir: Option<&str>| ProjectConfig {
            id: "p1".into(),
            name: "官网".into(),
            image_filter: String::new(),
            compose_file: "c.yml".into(),
            file_mappings: Vec::new(),
            service_overrides: Vec::new(),
            health_wait_secs: 0,
            pre_deploy_cmd: None,
            post_deploy_cmd: None,
            notify_webhook: None,
            source_compose_path: None,
            source_hash: None,
            remote_dir: dir.map(String::from),
            default_server_id: None,
            release_keep: None,
        };
        // 未配置 → 服务器目录(旧配置行为不变)
        assert_eq!(effective_remote_dir(&server, &mk_project(None)), "/home/henghao");
        // 空串/纯空白 → 同样回落(视为未配置)
        assert_eq!(effective_remote_dir(&server, &mk_project(Some(""))), "/home/henghao");
        assert_eq!(effective_remote_dir(&server, &mk_project(Some("   "))), "/home/henghao");
        // 配置了 → 用项目自己的目录
        assert_eq!(
            effective_remote_dir(&server, &mk_project(Some("/home/henghao/site"))),
            "/home/henghao/site"
        );
        // 前后空白被裁剪
        assert_eq!(
            effective_remote_dir(&server, &mk_project(Some("  /opt/app  "))),
            "/opt/app"
        );
    }

    #[test]
    fn test_project_config_legacy_json_defaults() {
        // 旧 projects.json(无 remote_dir / default_server_id)必须能反序列化,
        // 且两个新字段为 None(行为与历史一致)
        let legacy = r#"{
            "id": "p1", "name": "官网", "image_filter": "",
            "compose_file": "E:/cfg/stacks/u1/docker-compose.yml",
            "file_mappings": [],
            "service_overrides": [],
            "health_wait_secs": 0,
            "pre_deploy_cmd": null, "post_deploy_cmd": null, "notify_webhook": null
        }"#;
        let p: ProjectConfig = serde_json::from_str(legacy).unwrap();
        assert_eq!(p.remote_dir, None);
        assert_eq!(p.default_server_id, None);
        assert_eq!(p.source_compose_path, None);
        // 回落语义:旧配置取服务器目录
        let server = ServerConfig {
            id: "s1".into(),
            name: "s".into(),
            host: "h".into(),
            port: 22,
            username: "u".into(),
            auth: AuthConfig {
                auth_type: crate::config::AuthType::Password,
                key_path: None,
                password_enc: None,
                key_pass_enc: None,
            },
            remote_dir: "/opt/legacy".into(),
            host_key_sha256: None,
        };
        assert_eq!(effective_remote_dir(&server, &p), "/opt/legacy");
    }

    #[test]
    fn test_deploy_record_legacy_json_defaults() {
        // 旧 deployments.json(无 server_id/project_id)能反序列化,字段为 None
        let legacy = r#"{
            "ts": "2026-09-01 10:00:00", "mode": "stack",
            "server_name": "美国", "project_name": "官网",
            "images": ["a:1"], "success": true,
            "message": "部署完成", "duration_secs": 42
        }"#;
        let r: DeployRecord = serde_json::from_str(legacy).unwrap();
        assert_eq!(r.server_id, None);
        assert_eq!(r.project_id, None);
        assert_eq!(r.release_dir, None);
    }

    #[test]
    fn test_local_basename() {
        // Windows 与 POSIX 分隔符都支持,尾部斜杠去掉
        assert_eq!(local_basename("E:\\apps\\web"), Some("web".to_string()));
        assert_eq!(local_basename("/opt/data/"), Some("data".to_string()));
        assert_eq!(local_basename("C:/x/y/z.txt"), Some("z.txt".to_string()));
        assert_eq!(local_basename("web"), Some("web".to_string()));
        // 无法取名的输入 → None(调用方据此保持原行为)
        assert_eq!(local_basename(""), None);
        assert_eq!(local_basename("   "), None);
        assert_eq!(local_basename("/"), None);
    }

    #[test]
    fn test_source_content_hash_detects_change() {
        let dir = std::env::temp_dir().join(format!("dd-hash-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let compose = dir.join("docker-compose.yml");
        std::fs::write(&compose, "services:\n  web:\n    image: a:1\n").unwrap();
        let h1 = source_content_hash(&compose).unwrap();
        // 同内容 → 同哈希(可重复比对)
        assert_eq!(h1, source_content_hash(&compose).unwrap());

        // 改 compose → 哈希变化
        std::fs::write(&compose, "services:\n  web:\n    image: a:2\n").unwrap();
        let h2 = source_content_hash(&compose).unwrap();
        assert_ne!(h1, h2);

        // 改 .env(compose 未动)→ 也要变化(插值结果会变)
        std::fs::write(&compose, "services:\n  web:\n    image: a:1\n").unwrap();
        std::fs::write(dir.join(".env"), "TAG=2\n").unwrap();
        let h3 = source_content_hash(&compose).unwrap();
        assert_ne!(h1, h3);

        // 源不存在 → Err(调用方按"无法比对"降级,不误判为已变更)
        assert!(source_content_hash(&dir.join("nope.yml")).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_bundle_content_changed_ignores_compose_filename() {
        let dir = std::env::temp_dir().join(format!("dd-bundle-{}", uuid::Uuid::new_v4()));
        let src = dir.join("src");
        let dst = dir.join("dst");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        let body = "services:\n  web:\n    image: a:1\n";

        // 源叫 compose.yml,副本叫 docker-compose.yml —— 内容相同不应判为变更
        // (这正是 source_content_hash 按文件名入哈希的盲区)
        std::fs::write(src.join("compose.yml"), body).unwrap();
        std::fs::write(dst.join("docker-compose.yml"), body).unwrap();
        assert!(!bundle_content_changed(&src.join("compose.yml"), &dst.join("docker-compose.yml")).unwrap());

        // compose 内容不同 → 变更
        std::fs::write(dst.join("docker-compose.yml"), "services:\n  web:\n    image: a:2\n").unwrap();
        assert!(bundle_content_changed(&src.join("compose.yml"), &dst.join("docker-compose.yml")).unwrap());

        // 副本缺失 → 需同步
        assert!(bundle_content_changed(&src.join("compose.yml"), &dst.join("nope.yml")).unwrap());

        // .env 变化 → 变更(即使 compose 相同)
        std::fs::write(dst.join("docker-compose.yml"), body).unwrap();
        assert!(!bundle_content_changed(&src.join("compose.yml"), &dst.join("docker-compose.yml")).unwrap());
        std::fs::write(src.join(".env"), "TAG=2\n").unwrap();
        assert!(bundle_content_changed(&src.join("compose.yml"), &dst.join("docker-compose.yml")).unwrap());
        std::fs::write(dst.join(".env"), "TAG=2\n").unwrap();
        assert!(!bundle_content_changed(&src.join("compose.yml"), &dst.join("docker-compose.yml")).unwrap());

        // override 增删 → 变更
        std::fs::write(src.join("docker-compose.override.yml"), "services: {}\n").unwrap();
        assert!(bundle_content_changed(&src.join("compose.yml"), &dst.join("docker-compose.yml")).unwrap());
        std::fs::write(dst.join("docker-compose.override.yml"), "services: {}\n").unwrap();
        assert!(!bundle_content_changed(&src.join("compose.yml"), &dst.join("docker-compose.yml")).unwrap());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_update_reports_changed_before_overwriting_baseline() {
        // 回归:update_project_from_source 曾先写新哈希再复核状态,导致
        // 无论是否真的同步了内容都报 unchanged(用户看到"无改动"但文件已更新)。
        // 这里验证判定所用的"更新前副本 vs 源"语义:内容确有差异时必须为 changed。
        let dir = std::env::temp_dir().join(format!("dd-upd-{}", uuid::Uuid::new_v4()));
        let src = dir.join("src");
        let dst = dir.join("dst");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(src.join("docker-compose.yml"), "services:\n  web:\n    image: v2\n").unwrap();
        std::fs::write(dst.join("docker-compose.yml"), "services:\n  web:\n    image: v1\n").unwrap();
        // 更新前:内容不同 → 本次会带来改动(应报 changed)
        assert!(bundle_content_changed(&src.join("docker-compose.yml"), &dst.join("docker-compose.yml")).unwrap());
        // 模拟更新后(副本 == 源)→ 再比对为一致(基准已同步,后续不再报变更)
        std::fs::copy(src.join("docker-compose.yml"), dst.join("docker-compose.yml")).unwrap();
        assert!(!bundle_content_changed(&src.join("docker-compose.yml"), &dst.join("docker-compose.yml")).unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_project_source_status_states() {
        let mk = |compose: &str, path: Option<&str>, hash: Option<&str>| ProjectConfig {
            id: "p".into(),
            name: "n".into(),
            image_filter: String::new(),
            compose_file: compose.into(),
            file_mappings: Vec::new(),
            service_overrides: Vec::new(),
            health_wait_secs: 0,
            pre_deploy_cmd: None,
            post_deploy_cmd: None,
            notify_webhook: None,
            source_compose_path: path.map(String::from),
            source_hash: hash.map(String::from),
            remote_dir: None,
            default_server_id: None,
            release_keep: None,
        };
        // 未绑定源 → unbound;手工项目(远端相对路径)与导入项目都算 unbound,
        // 但 imported 不同,前端据此决定是否提示补绑
        let manual = mk("docker-compose.yml", None, None);
        let st = project_source_status(&manual);
        assert_eq!(st.state, "unbound");
        assert!(!st.imported, "远端相对路径 = 手工项目");
        assert!(!st.bound);

        // 导入项目(compose 在 config/stacks 下)→ imported=true(unbound 时前端提示绑定)
        let imported_compose = crate::config::config_dir()
            .join("stacks")
            .join("test-uuid")
            .join("docker-compose.yml");
        let imported = mk(&imported_compose.to_string_lossy(), None, None);
        let st = project_source_status(&imported);
        assert_eq!(st.state, "unbound");
        assert!(st.imported, "配置目录下副本 = 导入项目");

        // 源不存在 → missing
        let missing = mk(
            "docker-compose.yml",
            Some("E:/definitely/not/here/docker-compose.yml"),
            Some("x"),
        );
        assert_eq!(project_source_status(&missing).state, "missing");

        let dir = std::env::temp_dir().join(format!("dd-src-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let compose = dir.join("docker-compose.yml");
        std::fs::write(&compose, "services: {}\n").unwrap();

        // 已绑定但无哈希(手工改配置)→ 以当前内容为基准,报 unchanged 而非 unknown
        let no_hash = mk("docker-compose.yml", Some(&compose.to_string_lossy()), None);
        let st = project_source_status(&no_hash);
        assert_eq!(st.state, "unchanged");
        assert!(st.bound);

        // 哈希一致 → unchanged;不一致 → changed
        let h = source_content_hash(&compose).unwrap();
        let same = mk("docker-compose.yml", Some(&compose.to_string_lossy()), Some(&h));
        assert_eq!(project_source_status(&same).state, "unchanged");
        let diff = mk(
            "docker-compose.yml",
            Some(&compose.to_string_lossy()),
            Some("deadbeef"),
        );
        assert_eq!(project_source_status(&diff).state, "changed");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ===== deploy_notify_text(通知中心挂点文案)=====

    #[test]
    fn test_deploy_notify_text_success() {
        let record = DeployRecord {
            ts: "2026-09-05 12:00:00".into(),
            mode: MODE_SINGLE.into(),
            server_name: "生产".into(),
            project_name: "博客".into(),
            images: vec![],
            success: true,
            message: "部署完成".into(),
            duration_secs: 42,
            release_dir: None,
            server_id: None,
            project_id: None,
        };
        let (title, body) = deploy_notify_text(true, "部署完成", &Some(record));
        assert_eq!(title, "部署成功");
        assert!(body.contains("博客"));
        assert!(body.contains("生产"));
        assert!(body.contains("部署完成"));
        assert!(body.contains("42"));
    }

    #[test]
    fn test_deploy_notify_text_failure_and_cancel() {
        let record = DeployRecord {
            ts: String::new(),
            mode: MODE_STACK.into(),
            server_name: "s".into(),
            project_name: "p".into(),
            images: vec![],
            success: false,
            message: crate::errors::cancelled(),
            duration_secs: 3,
            release_dir: None,
            server_id: None,
            project_id: None,
        };
        // 取消(错误码 canceled)→ cancel 标题;正文剥标记展示纯文案
        let (title, body) = deploy_notify_text(false, &crate::errors::cancelled(), &Some(record));
        assert_eq!(title, "部署已取消");
        assert!(body.contains(CANCELLED_MSG));
        assert!(!body.contains("dderr"), "正文不得带码标记: {}", body);
        // 普通失败 → failure 标题
        let (title, _) = deploy_notify_text(false, "健康检查未通过", &None);
        assert_eq!(title, "部署失败");
        // panic 路径(无记录)→ 兜底正文
        let (_, body) = deploy_notify_text(false, "部署过程发生内部错误", &None);
        assert!(body.contains("部署详情缺失"));
    }

    // ===== hook_failure_result(钩子失败映射)=====

    #[test]
    fn test_hook_failure_result_cancel_passthrough() {
        // 取消错误(码 canceled)原样透传(Pre/Post 同口径):包装成其他文案会让
        // 收尾的 cancel 判定失配,把取消误报为部署失败
        let cancelled = crate::errors::cancelled();
        assert_eq!(
            hook_failure_result(HookKind::Pre, &cancelled),
            Some(cancelled.clone())
        );
        assert_eq!(
            hook_failure_result(HookKind::Post, &cancelled),
            Some(cancelled)
        );
    }

    #[test]
    fn test_hook_failure_result_pre_wraps_post_swallows() {
        // Pre 普通失败 → 「执行失败,部署中止」;Post 普通失败 → None(仅告警)
        assert_eq!(
            hook_failure_result(HookKind::Pre, "exit code 1"),
            Some("部署前钩子执行失败,部署中止: exit code 1".to_string())
        );
        assert_eq!(hook_failure_result(HookKind::Post, "exit code 1"), None);
    }

    // ===== rollback_notify_text(回滚通知文案)=====

    #[test]
    fn test_rollback_notify_text_success() {
        let record = DeployRecord {
            ts: "2026-09-05 12:00:00".into(),
            mode: MODE_ROLLBACK.into(),
            server_name: "生产".into(),
            project_name: "博客".into(),
            images: vec![],
            success: true,
            message: "回滚到 20260905120000".into(),
            duration_secs: 7,
            release_dir: None,
            server_id: None,
            project_id: None,
        };
        let message = record.message.clone();
        let (title, body) = rollback_notify_text(true, &message, &Some(record));
        assert_eq!(title, "回滚成功");
        assert!(body.contains("博客"));
        // 目标 release ts 随「回滚到 …」消息进入正文
        assert!(body.contains("20260905120000"));
        assert!(body.contains("7"));
    }

    #[test]
    fn test_rollback_notify_text_failure_and_cancel() {
        // 取消(错误码 canceled)→ cancel 标题;普通失败 → failure 标题
        let (title, _) = rollback_notify_text(false, &crate::errors::cancelled(), &None);
        assert_eq!(title, "回滚已取消");
        let (title, body) = rollback_notify_text(false, "回滚目标发布目录不存在", &None);
        assert_eq!(title, "回滚失败");
        // 无记录(panic/早期失败)→ 兜底正文
        assert!(body.contains("回滚详情缺失"));
    }

    // ===== remote_join =====

    #[test]
    fn test_remote_join_simple() {
        assert_eq!(
            remote_join("/opt/app", "docker-compose.yml"),
            "/opt/app/docker-compose.yml"
        );
    }

    #[test]
    fn test_remote_join_nested_rel() {
        assert_eq!(
            remote_join("/opt/app", "sql/init.sql"),
            "/opt/app/sql/init.sql"
        );
    }

    #[test]
    fn test_remote_join_leading_slash_no_escape() {
        // rel 以 '/' 开头时视为相对 remote_dir 仍拼接,不提供绝对路径逃逸
        assert_eq!(remote_join("/opt/app", "/abs/x"), "/opt/app/abs/x");
    }

    #[test]
    fn test_remote_join_base_trailing_slash() {
        assert_eq!(remote_join("/opt/app/", "y"), "/opt/app/y");
        assert_eq!(remote_join("/opt/app//", "a/b"), "/opt/app/a/b");
    }

    #[test]
    fn test_remote_join_empty_rel() {
        assert_eq!(remote_join("/opt/app", ""), "/opt/app");
    }

    #[test]
    fn test_remote_join_empty_base() {
        assert_eq!(remote_join("", "a.txt"), "/a.txt");
        assert_eq!(remote_join("/", "a.txt"), "/a.txt");
    }

    // ===== split_remote_file =====

    #[test]
    fn test_split_remote_file() {
        assert_eq!(
            split_remote_file("/opt/app/docker-compose.yml"),
            ("/opt/app".to_string(), "docker-compose.yml".to_string())
        );
        assert_eq!(
            split_remote_file("a.txt"),
            (String::new(), "a.txt".to_string())
        );
    }

    // ===== shell_single_quote =====

    #[test]
    fn test_shell_single_quote() {
        assert_eq!(shell_single_quote("/opt/app"), "'/opt/app'");
        assert_eq!(shell_single_quote("/opt/a'b"), "'/opt/a'\\''b'");
    }

    // ===== 单镜像 compose 路径 =====

    #[test]
    fn test_is_windows_absolute_path() {
        assert!(is_windows_absolute_path(
            r"E:\github\Docker-Deploy-SSH\docker-compose.yml"
        ));
        assert!(is_windows_absolute_path(
            "E:/github/Docker-Deploy-SSH/docker-compose.yml"
        ));
        assert!(is_windows_absolute_path(r"\\server\share\docker-compose.yml"));
        assert!(!is_windows_absolute_path("docker-compose.yml"));
        assert!(!is_windows_absolute_path("/opt/app/docker-compose.yml"));
    }

    #[test]
    fn test_single_image_command_uses_remote_compose_path() {
        let local = r"E:\github\Docker-Deploy-SSH\config\stacks\id\docker-compose.yml";
        let remote = remote_compose_path("/home/henghao");
        let cmd = compose_up_cmd("/home/henghao", &remote, &[]);
        assert!(!cmd.contains(local));
        assert_eq!(
            cmd,
            "cd '/home/henghao' && docker compose -f '/home/henghao/docker-compose.yml' up -d"
        );
    }

    // ===== resolve_password =====

    #[test]
    fn test_resolve_password_key_auth() {
        // Key 认证:一律 None,不使用密码
        assert_eq!(resolve_password(&AuthType::Key, None, None).unwrap(), None);
        assert_eq!(
            resolve_password(&AuthType::Key, Some("ignored"), Some("enc")).unwrap(),
            None
        );
    }

    #[test]
    fn test_resolve_password_plain_takes_priority() {
        assert_eq!(
            resolve_password(&AuthType::Password, Some("pw"), None).unwrap(),
            Some("pw".to_string())
        );
        // 前端输入的明文优先于已保存密文
        assert_eq!(
            resolve_password(&AuthType::Password, Some("fresh"), Some("enc")).unwrap(),
            Some("fresh".to_string())
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_resolve_password_fallback_to_enc() {
        let enc = crate::crypto::dpapi_protect("saved-pw").unwrap();
        assert_eq!(
            resolve_password(&AuthType::Password, None, Some(&enc)).unwrap(),
            Some("saved-pw".to_string())
        );
        // 空明文视为未输入,回退到已保存密文
        assert_eq!(
            resolve_password(&AuthType::Password, Some(""), Some(&enc)).unwrap(),
            Some("saved-pw".to_string())
        );
    }

    #[test]
    fn test_resolve_password_missing() {
        // 密码认证但既无明文也无密文 → 报错
        assert!(resolve_password(&AuthType::Password, None, None).is_err());
    }

    #[test]
    fn test_resolve_password_bad_enc() {
        // 密文无效(base64 非法)→ 报错
        assert!(resolve_password(&AuthType::Password, None, Some("不是base64!!")).is_err());
    }

    // ===== resolve_key_passphrase(阶段三:加密私钥口令)=====

    /// 构造仅含 auth 的最小 ServerConfig(供 resolve_key_passphrase 测试)。
    #[allow(dead_code)]
    fn key_pass_cfg(key_pass_enc: Option<String>) -> ServerConfig {
        ServerConfig {
            id: "s1".into(),
            name: "n".into(),
            host: "1.2.3.4".into(),
            port: 22,
            username: "root".into(),
            auth: AuthConfig {
                auth_type: AuthType::Key,
                key_path: Some("C:/k".into()),
                password_enc: None,
                key_pass_enc,
            },
            remote_dir: "/opt/app".into(),
            host_key_sha256: None,
        }
    }

    #[test]
    fn test_resolve_key_passphrase_none_when_absent() {
        // 未配置 key_pass_enc(旧版配置/未加密私钥)→ None,不报错
        assert_eq!(resolve_key_passphrase(&key_pass_cfg(None)).unwrap(), None);
        // 空串按未配置处理
        assert_eq!(resolve_key_passphrase(&key_pass_cfg(Some(String::new()))).unwrap(), None);
    }

    #[cfg(windows)]
    #[test]
    fn test_resolve_key_passphrase_dpapi_roundtrip() {
        let enc = dpapi_protect("key-pass-123").unwrap();
        assert_eq!(
            resolve_key_passphrase(&key_pass_cfg(Some(enc))).unwrap(),
            Some("key-pass-123".to_string())
        );
    }

    #[test]
    fn test_resolve_key_passphrase_bad_enc() {
        // 密文无效(base64 非法)→ 报错
        assert!(resolve_key_passphrase(&key_pass_cfg(Some("不是base64!!".into()))).is_err());
    }

    // ===== import_compose =====

    #[test]
    fn test_import_compose_copies_parse_and_saves() {
        // DD_CONFIG_DIR 是进程级环境变量,与 config 层测试共用锁串行执行
        let _guard = crate::config::TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-import-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());

        // 源 compose(文件名故意不是 docker-compose.yml)+ 同目录 .env
        let src_dir = dir.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let source = src_dir.join("my-stack.yaml");
        std::fs::write(
            &source,
            "name: demo\nservices:\n  web:\n    build: ./web\n    image: ${IMAGE}:v1\n  db:\n    image: postgres:16\n",
        )
        .unwrap();
        std::fs::write(src_dir.join(".env"), "IMAGE=myapp\n").unwrap();

        let project =
            import_compose(source.to_string_lossy().to_string(), "测试栈".into()).unwrap();

        // 副本位于 config/stacks/<uuid>/docker-compose.yml,内容与源一致
        let copy = PathBuf::from(&project.compose_file);
        assert!(copy.is_file(), "compose 副本应存在: {}", project.compose_file);
        assert_eq!(copy.file_name().unwrap().to_string_lossy(), "docker-compose.yml");
        let stacks_dir = dir.join("config").join("stacks");
        assert_eq!(
            copy.parent().unwrap().parent().unwrap(),
            stacks_dir.as_path(),
            "副本应在 config/stacks/<uuid>/ 下"
        );
        assert_eq!(
            std::fs::read_to_string(&copy).unwrap(),
            std::fs::read_to_string(&source).unwrap(),
            "副本内容应与源一致(原样复制,不做插值)"
        );
        // 同目录 .env 一并复制
        assert!(copy.parent().unwrap().join(".env").is_file(), ".env 副本应存在");
        // origin.json 记录导入来源的原始父目录名(供副本路径解析默认镜像名兜底)
        let origin: crate::stack::StackOrigin = serde_json::from_str(
            &std::fs::read_to_string(copy.parent().unwrap().join("origin.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(origin.dir_name, "src", "origin.json 应记录源的父目录名");

        // service_overrides 取解析默认:web=Local(build),db=Pull(仅 image)
        assert_eq!(project.service_overrides.len(), 2);
        let web = project.service_overrides.iter().find(|o| o.service == "web").unwrap();
        assert_eq!(web.mode, TransferMode::Local);
        let db = project.service_overrides.iter().find(|o| o.service == "db").unwrap();
        assert_eq!(db.mode, TransferMode::Pull);

        // 返回的 compose_file 指向副本而非源路径;配置已保存
        assert_ne!(project.compose_file, source.to_string_lossy().to_string());
        let cfg = load_config().unwrap();
        assert!(cfg.projects.iter().any(|p| p.id == project.id && p.name == "测试栈"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_import_compose_missing_file() {
        let err = import_compose("Z:/definitely/not/compose.yml".into(), "x".into()).unwrap_err();
        assert!(err.contains("不存在"), "实际: {}", err);
    }

    #[test]
    fn test_import_compose_invalid_yaml() {
        let _guard = crate::config::TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-import-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());
        let source = dir.join("bad.yml");
        std::fs::write(&source, "services: [unclosed\n").unwrap();
        let err = import_compose(source.to_string_lossy().to_string(), "x".into()).unwrap_err();
        std::fs::remove_dir_all(&dir).ok();
        // 解析失败不落盘:不应产生 stacks 目录
        assert!(err.contains("YAML"), "实际: {}", err);
        assert!(!dir.join("config").join("stacks").exists(), "解析失败不应创建栈目录");
    }

    #[test]
    fn test_import_compose_copies_override_files() {
        let _guard = crate::config::TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-import-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());

        // 源 compose + 同目录 override(合并后 web 的 image 以 override 为准)
        let src_dir = dir.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let source = src_dir.join("docker-compose.yml");
        std::fs::write(
            &source,
            "name: demo\nservices:\n  web:\n    build: ./web\n    image: myapp:1\n  db:\n    image: postgres:16\n",
        )
        .unwrap();
        std::fs::write(
            src_dir.join("compose.override.yaml"),
            "services:\n  web:\n    image: myapp:2\n",
        )
        .unwrap();
        // 非 override 命名的文件不应被复制
        std::fs::write(src_dir.join("other.yaml"), "services: {}\n").unwrap();

        let project =
            import_compose(source.to_string_lossy().to_string(), "override 栈".into()).unwrap();
        let copy_dir = PathBuf::from(&project.compose_file)
            .parent()
            .unwrap()
            .to_path_buf();

        // override 副本同名落在 stacks/<uuid>/ 下
        assert!(
            copy_dir.join("compose.override.yaml").is_file(),
            "override 副本应存在: {}",
            copy_dir.display()
        );
        assert!(
            !copy_dir.join("other.yaml").exists(),
            "非 override 文件不应被复制"
        );
        // 解析时已合并 override:web 的 image 以 override 为准(build 保留 → Local)
        let web = project.service_overrides.iter().find(|o| o.service == "web").unwrap();
        assert_eq!(web.mode, TransferMode::Local);
        let db = project.service_overrides.iter().find(|o| o.service == "db").unwrap();
        assert_eq!(db.mode, TransferMode::Pull);

        std::fs::remove_dir_all(&dir).ok();
    }

    // ===== 整栈部署:请求反序列化(前端契约,snake_case)=====

    /// 构造服务分类项的便捷函数。
    fn choice(service: &str, image: &str, mode: TransferMode) -> StackServiceChoice {
        StackServiceChoice {
            service: service.into(),
            image: image.into(),
            mode,
        }
    }

    #[test]
    fn test_stack_deploy_request_deserialize() {
        let json = r#"{
            "project_id": "p1",
            "server_id": "s1",
            "services": [
                {"service": "web", "image": "myapp:1", "mode": "Local"},
                {"service": "db", "image": "", "mode": "Pull"}
            ],
            "password_plain": null
        }"#;
        let req: StackDeployRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.project_id, "p1");
        assert_eq!(req.server_id, "s1");
        assert_eq!(req.services.len(), 2);
        assert_eq!(req.services[0].mode, TransferMode::Local);
        assert_eq!(req.services[1].mode, TransferMode::Pull);
        assert_eq!(req.services[1].image, "");
        assert_eq!(req.password_plain, None);
    }

    // ===== 步骤 1:validate_stack_choices =====

    #[test]
    fn test_validate_stack_choices_rejects_empty() {
        let err = validate_stack_choices(&[]).unwrap_err();
        assert!(err.contains("为空"), "实际: {}", err);
    }

    #[test]
    fn test_validate_stack_choices_local_image_required() {
        let services = vec![
            choice("web", "myapp:1", TransferMode::Local),
            choice("db", "   ", TransferMode::Local),
        ];
        let err = validate_stack_choices(&services).unwrap_err();
        assert!(err.contains("db"), "错误应含服务名: {}", err);
        assert!(err.contains("本地传输"), "实际: {}", err);
    }

    #[test]
    fn test_validate_stack_choices_ok_allows_empty_pull_image() {
        // Pull 类镜像为空合法(服务器自行拉取)
        let services = vec![
            choice("web", "myapp:1", TransferMode::Local),
            choice("db", "", TransferMode::Pull),
        ];
        validate_stack_choices(&services).unwrap();
    }

    // ===== 步骤 2:group_by_mode / sum_sizes =====

    #[test]
    fn test_group_by_mode() {
        let services = vec![
            choice("a", "a:1", TransferMode::Local),
            choice("b", "b:1", TransferMode::Pull),
            choice("c", "c:1", TransferMode::Local),
        ];
        let (local, pull) = group_by_mode(&services);
        let local_names: Vec<&str> = local.iter().map(|s| s.service.as_str()).collect();
        let pull_names: Vec<&str> = pull.iter().map(|s| s.service.as_str()).collect();
        assert_eq!(local_names, vec!["a", "c"]);
        assert_eq!(pull_names, vec!["b"]);
    }

    #[test]
    fn test_group_by_mode_all_pull() {
        // 全 Pull:local 为空,打包/装载/上传镜像包均跳过
        let services = vec![choice("db", "", TransferMode::Pull)];
        let (local, pull) = group_by_mode(&services);
        assert!(local.is_empty());
        assert_eq!(pull.len(), 1);
    }

    #[test]
    fn test_stack_record_images() {
        // 仅登记本地传输且镜像非空的服务,按服务顺序
        let services = vec![
            choice("web", "myapp:1", TransferMode::Local),
            choice("db", "", TransferMode::Pull),
            choice("cache", "redis:7", TransferMode::Local),
            choice("worker", "   ", TransferMode::Local),
        ];
        assert_eq!(
            stack_record_images(&services),
            vec!["myapp:1".to_string(), "redis:7".to_string()]
        );
        // 全 Pull → 空列表
        assert!(stack_record_images(&[choice("db", "", TransferMode::Pull)]).is_empty());
    }

    #[test]
    fn test_sum_sizes() {
        assert_eq!(sum_sizes(&[]), Some(0));
        assert_eq!(sum_sizes(&[Some(1), Some(2)]), Some(3));
        // 任一项未知 → 整体未知(跳过磁盘预检)
        assert_eq!(sum_sizes(&[Some(1), None, Some(2)]), None);
        // 溢出保护
        assert_eq!(sum_sizes(&[Some(u64::MAX), Some(1)]), None);
    }

    // ===== 步骤 3/4:releases 路径拼装 =====

    #[test]
    fn test_releases_dir() {
        assert_eq!(
            releases_dir("/opt/app", "20260829-101010"),
            "/opt/app/releases/20260829-101010"
        );
        // remote_dir 尾部斜杠被吸收
        assert_eq!(
            releases_dir("/opt/app/", "20260829-101010"),
            "/opt/app/releases/20260829-101010"
        );
    }

    #[test]
    fn test_remote_compose_path() {
        assert_eq!(
            remote_compose_path("/opt/app"),
            "/opt/app/docker-compose.yml"
        );
    }

    // ===== 步骤 5:compose_pull_cmd =====

    #[test]
    fn test_compose_pull_cmd() {
        assert_eq!(
            compose_pull_cmd(
                "/opt/app",
                "/opt/app/docker-compose.yml",
                &[],
                &["web".to_string(), "db".to_string()]
            ),
            "cd '/opt/app' && docker compose -f '/opt/app/docker-compose.yml' pull 'web' 'db'"
        );
        // 有 override:按检测顺序追加 -f(compose 后者覆盖前者)
        assert_eq!(
            compose_pull_cmd(
                "/opt/app",
                "/opt/app/docker-compose.yml",
                &["compose.override.yaml".to_string(), "docker-compose.override.yml".to_string()],
                &["web".to_string()]
            ),
            "cd '/opt/app' && docker compose -f '/opt/app/docker-compose.yml' -f 'compose.override.yaml' -f 'docker-compose.override.yml' pull 'web'"
        );
    }

    #[test]
    fn test_compose_pull_cmd_quotes_block_injection() {
        // 服务名内嵌单引号被 '\'' 转义,无法逃出引号注入额外命令
        assert_eq!(
            compose_pull_cmd(
                "/opt/app",
                "/opt/app/docker-compose.yml",
                &[],
                &["a'; rm -rf /".to_string()]
            ),
            "cd '/opt/app' && docker compose -f '/opt/app/docker-compose.yml' pull 'a'\\''; rm -rf /'"
        );
        // override 文件名同样转义
        assert_eq!(
            compose_pull_cmd(
                "/opt/app",
                "/opt/app/docker-compose.yml",
                &["o'.yaml".to_string()],
                &["web".to_string()]
            ),
            "cd '/opt/app' && docker compose -f '/opt/app/docker-compose.yml' -f 'o'\\''.yaml' pull 'web'"
        );
    }

    // ===== 步骤 6:compose_up_cmd =====

    #[test]
    fn test_compose_up_cmd() {
        assert_eq!(
            compose_up_cmd("/opt/app", "/opt/app/docker-compose.yml", &[]),
            "cd '/opt/app' && docker compose -f '/opt/app/docker-compose.yml' up -d"
        );
        assert_eq!(
            compose_up_cmd(
                "/opt/app",
                "/opt/app/docker-compose.yml",
                &["compose.override.yaml".to_string()]
            ),
            "cd '/opt/app' && docker compose -f '/opt/app/docker-compose.yml' -f 'compose.override.yaml' up -d"
        );
    }

    // ===== Task 5:augment_pull_error 私有仓库认证提示 =====

    #[test]
    fn test_augment_pull_error_auth_variants() {
        for fragment in [
            "unauthorized: authentication required",
            "HTTP 401 Unauthorized",
            "denied: requested access to the resource is denied",
            "_ERROR: PERMISSION DENIED_",
        ] {
            let err = format!("远端命令执行失败(退出码 1): pull ({})", fragment);
            let msg = augment_pull_error(&err);
            assert!(
                msg.contains("检测到私有仓库认证问题,请先在服务器上 docker login 对应 registry"),
                "应追加登录提示: {}", msg
            );
            assert!(msg.starts_with(&err), "原错误应保留在前: {}", msg);
        }
    }

    #[test]
    fn test_augment_pull_error_other_failure_unchanged() {
        let err = "远端命令执行失败(退出码 1): pull (no such host)";
        assert_eq!(augment_pull_error(err), err);
        assert_eq!(augment_pull_error(""), "");
    }

    // ===== 收尾:cleanup_releases_cmd =====

    #[test]
    fn test_cleanup_releases_cmd() {
        // keep = 5(历史默认):tail -n +6 起删
        assert_eq!(
            cleanup_releases_cmd("/opt/app", 5),
            "ls -1dt '/opt/app/releases'/*/ | tail -n +6 | xargs -r rm -rf"
        );
        // 项目自定义保留数:tail 起点 = keep + 1
        assert_eq!(
            cleanup_releases_cmd("/opt/app", 2),
            "ls -1dt '/opt/app/releases'/*/ | tail -n +3 | xargs -r rm -rf"
        );
        // keep = 0:删除全部历史归档(tail -n +1)
        assert_eq!(
            cleanup_releases_cmd("/opt/app", 0),
            "ls -1dt '/opt/app/releases'/*/ | tail -n +1 | xargs -r rm -rf"
        );
        // 上限值不产生异常(tail 起点 = 51)
        assert_eq!(
            cleanup_releases_cmd("/opt/app", 50),
            "ls -1dt '/opt/app/releases'/*/ | tail -n +51 | xargs -r rm -rf"
        );
    }

    #[test]
    fn test_cleanup_releases_cmd_escapes_quote() {
        assert_eq!(
            cleanup_releases_cmd("/op't", 5),
            "ls -1dt '/op'\\''t/releases'/*/ | tail -n +6 | xargs -r rm -rf"
        );
    }

    #[test]
    fn test_release_keep_of_fallback_and_clamp() {
        let mk = |keep: Option<u32>| ProjectConfig {
            id: "p".into(),
            name: "n".into(),
            image_filter: String::new(),
            compose_file: "c.yml".into(),
            file_mappings: Vec::new(),
            service_overrides: Vec::new(),
            health_wait_secs: 0,
            pre_deploy_cmd: None,
            post_deploy_cmd: None,
            notify_webhook: None,
            source_compose_path: None,
            source_hash: None,
            remote_dir: None,
            default_server_id: None,
            release_keep: keep,
        };
        // 未配置(旧配置)→ 默认 5(历史行为不变)
        assert_eq!(crate::config::release_keep_of(&mk(None)), 5);
        // 显式值被尊重,含边界 0
        assert_eq!(crate::config::release_keep_of(&mk(Some(0))), 0);
        assert_eq!(crate::config::release_keep_of(&mk(Some(1))), 1);
        assert_eq!(crate::config::release_keep_of(&mk(Some(20))), 20);
        // 超上限被夹到 50(防手改配置写入过大值导致 tail 参数异常)
        assert_eq!(crate::config::release_keep_of(&mk(Some(999))), 50);
    }

    // ===== 单镜像步骤 5.2:docker_tag_cmd =====

    #[test]
    fn test_docker_tag_cmd() {
        assert_eq!(
            docker_tag_cmd("myapp:20260829-143000", "myapp:latest"),
            "docker tag 'myapp:20260829-143000' 'myapp:latest'"
        );
    }

    #[test]
    fn test_docker_tag_cmd_escapes_quote() {
        assert_eq!(
            docker_tag_cmd("my'app:20260829", "my'app:latest"),
            "docker tag 'my'\\''app:20260829' 'my'\\''app:latest'"
        );
    }

    // ===== Task 2:远端磁盘预检命令拼装 =====

    #[test]
    fn test_docker_root_cmd() {
        assert_eq!(docker_root_cmd(), "docker info -f '{{.DockerRootDir}}'");
    }

    #[test]
    fn test_df_free_gb_cmd() {
        assert_eq!(
            df_free_gb_cmd("/var/lib/docker"),
            "df -PBG '/var/lib/docker' | tail -1 | awk '{print $4}'"
        );
    }

    #[test]
    fn test_df_free_gb_cmd_escapes_quote() {
        // 路径内嵌单引号被 '\'' 转义,无法逃出引号注入额外命令
        assert_eq!(
            df_free_gb_cmd("/var/li'b"),
            "df -PBG '/var/li'\\''b' | tail -1 | awk '{print $4}'"
        );
    }

    #[test]
    fn test_parse_df_gb() {
        assert_eq!(parse_df_gb("30G\n"), Some(30.0));
        assert_eq!(parse_df_gb("  12 "), Some(12.0));
        assert_eq!(parse_df_gb("0.5"), Some(0.5));
        // 空输出 / 非数字(BusyBox 等口径不一致)→ None,调用方跳过预检
        assert_eq!(parse_df_gb(""), None);
        assert_eq!(parse_df_gb("   \n"), None);
        assert_eq!(parse_df_gb("N/A"), None);
    }

    // ===== Task 2:precheck_remote_disk 判定(Ok / None 跳过 / 不足)=====

    #[test]
    fn test_precheck_remote_disk_ok() {
        assert!(precheck_remote_disk(Some(20.0), 15 * 1024 * 1024 * 1024).is_ok());
        // 恰好等于需求(边界)也通过
        assert!(precheck_remote_disk(Some(15.0), 15 * 1024 * 1024 * 1024).is_ok());
        // 需求为 0(如全 Pull)恒通过
        assert!(precheck_remote_disk(Some(0.0), 0).is_ok());
    }

    #[test]
    fn test_precheck_remote_disk_none_skips() {
        // 无法获取剩余空间 → 跳过预检(告警由调用方负责)
        assert!(precheck_remote_disk(None, u64::MAX).is_ok());
    }

    #[test]
    fn test_precheck_remote_disk_insufficient() {
        let err = precheck_remote_disk(Some(10.0), 15 * 1024 * 1024 * 1024).unwrap_err();
        assert!(err.contains("磁盘剩余空间不足"), "实际: {}", err);
        assert!(err.contains("15.0"), "错误应含所需 GB: {}", err);
        assert!(err.contains("10.0"), "错误应含实际 GB: {}", err);
        assert!(err.contains("清理服务器磁盘"), "实际: {}", err);
    }

    // ===== Task 2:prune_cmd =====

    #[test]
    fn test_prune_cmd() {
        assert_eq!(
            prune_cmd(),
            "docker image prune -f; docker container prune -f"
        );
    }

    // ===== Task 4 修复轮:镜像包上传失败后同路径重试一次 =====

    #[test]
    fn test_upload_retry_failure_msg_keeps_both_errors() {
        // 重试失败时报错应同时携带两次失败信息,便于对照断点与失败原因
        let msg = upload_retry_failure_msg("SFTP 写入远端文件失败 (…)", "SSH 连接失败");
        assert!(msg.contains("重试仍失败"), "实际: {}", msg);
        assert!(msg.contains("SFTP 写入远端文件失败"), "应含重试错误: {}", msg);
        assert!(msg.contains("首次失败:SSH 连接失败"), "应含首次错误: {}", msg);
    }

    // ===== Task 3:钩子命令拼装 =====

    #[test]
    fn test_hook_cmd() {
        assert_eq!(
            hook_cmd("/opt/app", "docker image prune -f"),
            "cd '/opt/app' && ( docker image prune -f )"
        );
    }

    #[test]
    fn test_hook_cmd_escapes_remote_dir_quote() {
        // remote_dir 单引号转义;钩子命令是用户配置的可信复合命令,原样拼入
        // (支持 && / ; / 重定向,不整体加引号 —— 非防注入边界,见 hook_cmd 文档)
        assert_eq!(
            hook_cmd("/op't", "a && b; c > /tmp/log"),
            "cd '/op'\\''t' && ( a && b; c > /tmp/log )"
        );
    }

    // ===== Task 3:compose ps / logs 命令拼装 =====

    #[test]
    fn test_compose_ps_json_cmd() {
        assert_eq!(
            compose_ps_json_cmd("/opt/app", "/opt/app/docker-compose.yml", &[]),
            "cd '/opt/app' && docker compose -f '/opt/app/docker-compose.yml' ps --all --format json"
        );
        // override 与 pull/up 同序追加 -f:override-only 服务也进入健康判定
        assert_eq!(
            compose_ps_json_cmd(
                "/opt/app",
                "/opt/app/docker-compose.yml",
                &["compose.override.yaml".to_string()]
            ),
            "cd '/opt/app' && docker compose -f '/opt/app/docker-compose.yml' -f 'compose.override.yaml' ps --all --format json"
        );
    }

    #[test]
    fn test_compose_ps_json_cmd_escapes_quote() {
        // 路径内嵌单引号被 '\'' 转义,无法逃出引号注入额外命令
        assert_eq!(
            compose_ps_json_cmd("/opt/app", "/op't.yml", &[]),
            "cd '/opt/app' && docker compose -f '/op'\\''t.yml' ps --all --format json"
        );
    }

    #[test]
    fn test_compose_logs_cmd() {
        assert_eq!(
            compose_logs_cmd("/opt/app", "./docker-compose.yml", &[]),
            "cd '/opt/app' && docker compose -f './docker-compose.yml' logs --tail 50"
        );
        assert_eq!(
            compose_logs_cmd(
                "/opt/app",
                "./docker-compose.yml",
                &["compose.override.yml".to_string()]
            ),
            "cd '/opt/app' && docker compose -f './docker-compose.yml' -f 'compose.override.yml' logs --tail 50"
        );
    }

    // ===== Task 3:health_verdict 判定 =====

    /// 构造一行 `compose ps --format json` 输出
    /// (health/exit_code 传 None 表示不带该字段)。
    fn ps_line(service: &str, state: &str, health: Option<&str>, exit_code: Option<i64>) -> String {
        let mut json = format!(r#"{{"Service":"{}","State":"{}""#, service, state);
        if let Some(h) = health {
            json.push_str(&format!(r#","Health":"{}""#, h));
        }
        if let Some(c) = exit_code {
            json.push_str(&format!(r#","ExitCode":{}"#, c));
        }
        json.push('}');
        json
    }

    /// String 行列表转 `&str` 切片(临时 String 需先绑定再借用)。
    fn as_lines(raw: &[String]) -> Vec<&str> {
        raw.iter().map(String::as_str).collect()
    }

    #[test]
    fn test_health_verdict_all_running_pass() {
        let raw = vec![
            ps_line("web", "running", None, None),
            ps_line("db", "running", Some(""), None),
            ps_line("cache", "running", Some("healthy"), Some(0)),
        ];
        assert_eq!(health_verdict(&as_lines(&raw)), HealthVerdict::Pass);
    }

    #[test]
    fn test_health_verdict_exited_fails_fast() {
        // exited 且无 ExitCode 字段(版本差异)→ 保守按失败(宁误报不漏报)
        let raw = vec![
            ps_line("web", "running", None, None),
            ps_line("db", "exited", None, None),
        ];
        assert_eq!(
            health_verdict(&as_lines(&raw)),
            HealthVerdict::Unhealthy {
                service: "db".to_string(),
                state: "exited".to_string()
            }
        );
    }

    #[test]
    fn test_health_verdict_exited_nonzero_exit_code_unhealthy() {
        // exited 且 ExitCode≠0 → 立即失败,状态注明"非零退出"
        let raw = vec![
            ps_line("web", "running", None, None),
            ps_line("db", "exited", None, Some(1)),
        ];
        assert_eq!(
            health_verdict(&as_lines(&raw)),
            HealthVerdict::Unhealthy {
                service: "db".to_string(),
                state: "exited(非零退出)".to_string()
            }
        );
    }

    #[test]
    fn test_health_verdict_exited_zero_exit_code_pending() {
        // exited 且 ExitCode==0(一次性服务正常退出)→ 不算失败,继续轮询,
        // pending 展示"已退出(退出码 0)"并置 exited_zero(预算耗尽时报错
        // 据此提示关闭健康检查)
        let raw = vec![
            ps_line("web", "running", None, None),
            ps_line("job", "exited", None, Some(0)),
        ];
        assert_eq!(
            health_verdict(&as_lines(&raw)),
            HealthVerdict::Indeterminate {
                pending: Some(("job".to_string(), "已退出(退出码 0)".to_string())),
                exited_zero: true
            }
        );
    }

    #[test]
    fn test_health_verdict_exited_missing_exit_code_unhealthy() {
        // 仅 exited 容器且无 ExitCode 字段 → 保守按失败
        let raw = vec![ps_line("db", "exited", Some(""), None)];
        assert_eq!(
            health_verdict(&as_lines(&raw)),
            HealthVerdict::Unhealthy {
                service: "db".to_string(),
                state: "exited".to_string()
            }
        );
    }

    #[test]
    fn test_health_verdict_restarting_and_dead_fail_fast() {
        for state in ["restarting", "dead"] {
            let raw = vec![ps_line("db", state, None, None)];
            assert_eq!(
                health_verdict(&as_lines(&raw)),
                HealthVerdict::Unhealthy {
                    service: "db".to_string(),
                    state: state.to_string()
                }
            );
        }
    }

    #[test]
    fn test_health_verdict_blank_or_garbage_indeterminate() {
        // 空行 / 全空白 / 非 JSON 输出(旧版 compose、警告行等)→ 无法判定,继续轮询
        assert_eq!(
            health_verdict(&[""]),
            HealthVerdict::Indeterminate { pending: None, exited_zero: false }
        );
        assert_eq!(
            health_verdict(&["   "]),
            HealthVerdict::Indeterminate { pending: None, exited_zero: false }
        );
        assert_eq!(
            health_verdict(&[r#"time="2026-08-30" level=warning msg="x""#]),
            HealthVerdict::Indeterminate { pending: None, exited_zero: false }
        );
    }

    #[test]
    fn test_health_verdict_health_three_states() {
        // 无 Health 字段(无 healthcheck)→ Pass
        let raw = vec![ps_line("web", "running", None, None)];
        assert_eq!(health_verdict(&as_lines(&raw)), HealthVerdict::Pass);
        // Health="healthy" → Pass
        let raw = vec![ps_line("web", "running", Some("healthy"), None)];
        assert_eq!(health_verdict(&as_lines(&raw)), HealthVerdict::Pass);
        // Health="starting" → 尚未就绪,继续轮询(pending 展示 Health)
        let raw = vec![ps_line("web", "running", Some("starting"), None)];
        assert_eq!(
            health_verdict(&as_lines(&raw)),
            HealthVerdict::Indeterminate {
                pending: Some(("web".to_string(), "starting".to_string())),
                exited_zero: false
            }
        );
        // Health="unhealthy" → 未通过,继续轮询(预算耗尽时报错展示该状态)
        let raw = vec![ps_line("web", "running", Some("unhealthy"), None)];
        assert_eq!(
            health_verdict(&as_lines(&raw)),
            HealthVerdict::Indeterminate {
                pending: Some(("web".to_string(), "unhealthy".to_string())),
                exited_zero: false
            }
        );
    }

    #[test]
    fn test_health_verdict_not_running_pending() {
        // 非终态且非 running(created/paused)→ 继续轮询,带出服务与容器状态
        let raw = vec![ps_line("web", "created", None, None)];
        assert_eq!(
            health_verdict(&as_lines(&raw)),
            HealthVerdict::Indeterminate {
                pending: Some(("web".to_string(), "created".to_string())),
                exited_zero: false
            }
        );
    }

    #[test]
    fn test_health_verdict_json_array_format() {
        // 旧版 compose 一次性输出 JSON 数组 → 同样可解析
        let lines = vec![
            r#"[{"Service":"web","State":"running"},{"Service":"db","State":"running","Health":"healthy"}]"#,
        ];
        assert_eq!(health_verdict(&lines), HealthVerdict::Pass);
    }

    #[test]
    fn test_health_verdict_falls_back_to_name_field() {
        // 缺 Service 字段时回退容器 Name
        let lines = vec![r#"{"Name":"app-db-1","State":"exited"}"#];
        assert_eq!(
            health_verdict(&lines),
            HealthVerdict::Unhealthy {
                service: "app-db-1".to_string(),
                state: "exited".to_string()
            }
        );
    }

    #[test]
    fn test_health_verdict_health_object_status() {
        // Health 为嵌套对象时取其 Status 字段
        let lines = vec![r#"{"Service":"web","State":"running","Health":{"Status":"healthy"}}"#];
        assert_eq!(health_verdict(&lines), HealthVerdict::Pass);
    }

    // ===== Task 6:classify_change 部署变更分类(六例)=====

    #[test]
    fn test_classify_change_pull_mode_always_pull() {
        // 例 1:Pull 类一律 "Pull",不看本地/远端状态
        assert_eq!(
            classify_change(
                &TransferMode::Pull,
                true,
                Some("sha256:a"),
                Some("sha256:a"),
                Some("old:1")
            ),
            "Pull"
        );
        assert_eq!(
            classify_change(&TransferMode::Pull, false, None, None, None),
            "Pull"
        );
    }

    #[test]
    fn test_classify_change_local_image_missing_absent() {
        // 例 2:Local 且本地不存在该 repo:tag → "Absent"
        assert_eq!(
            classify_change(&TransferMode::Local, false, Some("sha256:a"), None, None),
            "Absent"
        );
        assert_eq!(
            classify_change(&TransferMode::Local, false, None, None, Some("old:1")),
            "Absent"
        );
    }

    #[test]
    fn test_classify_change_remote_missing_create() {
        // 例 3:远端无该镜像、也无现存容器 → 全新创建
        assert_eq!(
            classify_change(&TransferMode::Local, true, None, Some("sha256:abc"), None),
            "Create"
        );
    }

    #[test]
    fn test_classify_change_remote_missing_with_container_recreate() {
        // 例 4:远端无该镜像但有现存容器(旧版在跑)→ 重建
        assert_eq!(
            classify_change(
                &TransferMode::Local,
                true,
                None,
                Some("sha256:abc"),
                Some("myapp:old")
            ),
            "Recreate"
        );
    }

    #[test]
    fn test_classify_change_same_id_unchanged() {
        // 例 5:远端镜像 ID 与本地一致(容忍 sha256: 前缀与大小写差异)→ 不变
        assert_eq!(
            classify_change(
                &TransferMode::Local,
                true,
                Some("sha256:ABC123"),
                Some("abc123"),
                Some("myapp:1")
            ),
            "Unchanged"
        );
    }

    #[test]
    fn test_classify_change_different_id_recreate() {
        // 例 6:ID 不同 → 镜像已更新,重建
        assert_eq!(
            classify_change(
                &TransferMode::Local,
                true,
                Some("sha256:aaa"),
                Some("sha256:bbb"),
                Some("myapp:1")
            ),
            "Recreate"
        );
    }

    // ===== 智能传输:same_image_id 完整 ID 口径 =====

    #[test]
    fn test_same_image_id_full_64_hex_equal() {
        // 远端 --no-trunc 输出(sha256: 前缀 + 完整 64 位)vs 本地
        // image_id_by_ref 输出(剥前缀后的完整 64 位)→ 相等(跳过判定的主口径)
        let full = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert!(same_image_id(
            &format!("sha256:{}", full),
            full
        ));
        assert!(same_image_id(
            &format!("sha256:{}", full),
            &format!("sha256:{}", full)
        ));
    }

    #[test]
    fn test_same_image_id_case_and_prefix_mixed() {
        // 大小写差异、sha256: 前缀有无混合,均视为相等
        assert!(same_image_id("sha256:ABCDEF123456", "abcdef123456"));
        assert!(same_image_id("ABCDEF123456", "sha256:abcdef123456"));
        assert!(same_image_id("  sha256:abc123  ", "ABC123"));
    }

    #[test]
    fn test_same_image_id_truncated_vs_full_not_equal() {
        // 回归:12 位截断 ID(旧 REMOTE_IMAGES_CMD 口径)与完整 64 位 ID
        // 不相等 —— 跳过判定数据源必须用 REMOTE_IMAGES_CMD_FULL
        let full = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert!(!same_image_id("0123456789abcdef", full));
    }

    #[test]
    fn test_same_image_id_empty_not_equal() {
        // 任一为空视为不等(避免空串误判"未变化")
        assert!(!same_image_id("", "abc"));
        assert!(!same_image_id("sha256:", "abc"));
        assert!(!same_image_id("", ""));
    }

    // ===== 镜像 ID 查询命令与解析(迁移路径取 ID 的专用口径)=====

    #[test]
    fn test_docker_inspect_id_cmd_has_format_and_quotes() {
        // 回归:迁移路径曾用不带 --format 的 docker_inspect_cmd 取 ID,
        // 输出是 JSON 数组 → 解析恒失败 → 每个镜像都误报「源服务器上不存在」。
        let cmd = docker_inspect_id_cmd("myapp:latest");
        assert!(cmd.contains("--format"), "必须带 --format 才能取到 ID 单值");
        assert!(cmd.contains("{{.Id}}"), "格式串须为 {{{{.Id}}}}");
        assert!(cmd.contains("'myapp:latest'"), "引用须单引号包裹");
    }

    #[test]
    fn test_docker_inspect_id_cmd_quotes_escaped_for_injection() {
        // 引用含单引号时按 '\'' 转义,不产生注入面
        let cmd = docker_inspect_id_cmd("a'b");
        assert!(!cmd.contains("'a'b'"), "未转义的原样拼接会破坏引号");
        assert!(cmd.contains(r"'\''"), "单引号应转义为 '\\''");
    }

    #[test]
    fn test_parse_inspect_id_strips_prefix_and_trim() {
        let full = "sha256:0123456789abcdef";
        assert_eq!(parse_inspect_id(&format!("{}\n", full)), Some("0123456789abcdef".into()));
        assert_eq!(parse_inspect_id("sha256:abc"), Some("abc".into()));
        // 已无前缀原样返回(兼容不同 docker 版本输出)
        assert_eq!(parse_inspect_id("abc123"), Some("abc123".into()));
    }

    #[test]
    fn test_parse_inspect_id_rejects_json_shape_and_empty() {
        // 误用无 --format 的 inspect(JSON 数组)时必须返回 None,
        // 而不是把 "[\n    {\n        \"Id\": \"sha256:abcdef\",\n        \"RepoTags\": null\n    }\n]" 当镜像 ID 参与比较
        let json_output = "[\n    {\n        \"Id\": \"sha256:abcdef\",\n        \"RepoTags\": null\n    }\n]";
        assert_eq!(parse_inspect_id(json_output), None);
        assert_eq!(parse_inspect_id("{\"Id\": \"sha256:abc\"}"), None);
        assert_eq!(parse_inspect_id(""), None);
        assert_eq!(parse_inspect_id("   \n  "), None);
    }

    // ===== 项目级远程目录检测/创建(第六批)=====

    #[test]
    fn test_test_dir_cmd_quotes_and_escapes() {
        // 目录存在性检查命令:`test -d '<path>'`,路径单引号包裹防注入
        let cmd = test_dir_cmd("/home/henghao/site");
        assert_eq!(cmd, "test -d '/home/henghao/site'");
        // 含单引号与 shell 元字符时经 '\'' 转义,不留注入面
        let evil = test_dir_cmd("/tmp/a'; rm -rf /; echo '");
        assert!(!evil.contains("a'; rm"), "未转义的引号会闭合字符串: {}", evil);
        assert!(evil.contains(r"'\''"), "应转义为 '\\'': {}", evil);
        let dollar = test_dir_cmd("/tmp/x$(whoami)`id`");
        assert!(dollar.contains("'/tmp/x$(whoami)`id`'"), "{}", dollar);
    }

    #[test]
    fn test_mkdir_p_cmd_quotes_and_escapes() {
        // 创建目录命令:`mkdir -p '<path>'`
        let cmd = mkdir_p_cmd("/home/henghao/site");
        assert_eq!(cmd, "mkdir -p '/home/henghao/site'");
        let evil = mkdir_p_cmd("/tmp/a'; rm -rf /; echo '");
        assert!(!evil.contains("a'; rm"), "{}", evil);
        assert!(evil.contains(r"'\''"), "{}", evil);
    }


    // ===== Task 6:webhook 载荷序列化 =====

    #[test]
    fn test_webhook_payload_fields() {
        let record = DeployRecord {
            ts: "2026-08-29 10:00:00".into(),
            mode: MODE_STACK.into(),
            server_name: "生产服务器".into(),
            project_name: "博客".into(),
            images: vec!["web:1".into()],
            success: true,
            message: "部署完成".into(),
            duration_secs: 42,
            release_dir: None,
            server_id: None,
            project_id: None,
        };
        let v: serde_json::Value = serde_json::from_str(&webhook_payload(&record)).unwrap();
        assert_eq!(v["event"], "deploy");
        assert_eq!(v["success"], true);
        assert_eq!(v["message"], "部署完成");
        assert_eq!(v["server"], "生产服务器");
        assert_eq!(v["project"], "博客");
        assert_eq!(v["duration_secs"], 42);
        assert_eq!(v["ts"], "2026-08-29 10:00:00");

        // 失败记录同样携带完整字段(success=false)
        let mut failed = record.clone();
        failed.success = false;
        failed.message = "部署失败:连接超时".into();
        let v: serde_json::Value = serde_json::from_str(&webhook_payload(&failed)).unwrap();
        assert_eq!(v["success"], false);
        assert_eq!(v["message"], "部署失败:连接超时");
        assert_eq!(v["event"], "deploy");
    }

    // ===== Task 6:远端容器查询命令与解析 =====

    #[test]
    fn test_remote_dir_basename() {
        assert_eq!(remote_dir_basename("/opt/app"), "app");
        assert_eq!(remote_dir_basename("/opt/app/"), "app");
        assert_eq!(remote_dir_basename("app"), "app");
        assert_eq!(remote_dir_basename("/"), "");
        assert_eq!(remote_dir_basename(""), "");
    }

    #[test]
    fn test_compose_containers_cmd() {
        assert_eq!(
            compose_containers_cmd("/opt/app"),
            "docker ps -a --filter 'label=com.docker.compose.project=app' --format '{{json .}}'"
        );
        // 项目名(基名)内嵌单引号被 '\'' 转义,无法逃出引号注入额外命令
        assert_eq!(
            compose_containers_cmd("/op't"),
            "docker ps -a --filter 'label=com.docker.compose.project=op'\\''t' --format '{{json .}}'"
        );
    }

    #[test]
    fn test_parse_container_line() {
        let line = r#"{"Command":"nginx","CreatedAt":"2026-08-30 10:00:00 +0800 CST","ID":"abc123","Image":"myapp:1","Labels":"com.docker.compose.project=demo,com.docker.compose.service=web","Names":"demo-web-1","State":"running"}"#;
        assert_eq!(
            parse_container_line(line),
            Some(("web".to_string(), "myapp:1".to_string()))
        );
        // 无 compose 服务标签(手动 docker run 的容器)→ None
        assert_eq!(parse_container_line(r#"{"Image":"x","Labels":"foo=bar"}"#), None);
        // 非 JSON 行 → None
        assert_eq!(parse_container_line("oops"), None);
    }

    // ===== 修复轮:webhook URL 日志脱敏 =====

    #[test]
    fn test_url_host_for_log_sanitizes_token() {
        // query 内嵌 token(飞书/钉钉机器人地址形态):只留 host
        assert_eq!(
            url_host_for_log("https://open.feishu.cn/open-apis/bot/v2/hook?token=secret123"),
            "open.feishu.cn"
        );
        // userinfo 内嵌凭据:取 @ 之后的 host[:port]
        assert_eq!(
            url_host_for_log("https://user:pass@hook.example.com:8443/x"),
            "hook.example.com:8443"
        );
        // 无 path/query
        assert_eq!(url_host_for_log("http://example.com"), "example.com");
        // 解析不出 host(非 URL / 空 authority)→ 占位符
        assert_eq!(url_host_for_log("not-a-url"), "<unparseable-url>");
        assert_eq!(url_host_for_log("https://"), "<unparseable-url>");
        assert_eq!(url_host_for_log(""), "<unparseable-url>");
    }

    #[test]
    fn test_webhook_error_detail_no_url() {
        // Status 错误:只落状态码,不内嵌 URL(ureq 的 Display 会带响应 URL)
        let resp = ureq::Response::new(404, "Not Found", "body").unwrap();
        let detail = webhook_error_detail(&ureq::Error::Status(404, resp));
        assert_eq!(detail, "HTTP 状态码 404");
        assert!(!detail.contains("http"), "不应包含 URL: {}", detail);
    }

    // ===== 阶段六:部署断点续传(纯函数单测)=====

    /// 构造单镜像断点产物(测试便捷函数)。
    fn single_art() -> SingleResumeArtifacts {
        SingleResumeArtifacts {
            origin_ref: "myapp:latest".into(),
            repository: "myapp".into(),
            use_date_tag: true,
            image_ref: Some("myapp:20260906-101010".into()),
            tar_local: Some("C:\\Temp\\abc.tar.gz".into()),
            tar_name: Some("abc.tar.gz".into()),
            skip_unchanged: false,
        }
    }

    /// 构造整栈断点产物(测试便捷函数)。
    fn stack_art() -> StackResumeArtifacts {
        StackResumeArtifacts {
            services: vec![
                choice("web", "myapp:1", TransferMode::Local),
                choice("db", "", TransferMode::Pull),
            ],
            unchanged: vec![false],
            skip_unchanged: true,
            force_archive: false,
            release_ts: Some("20260906-101010".into()),
            files: vec!["a.tar.gz".into()],
            locals: vec!["C:\\Temp\\a.tar.gz".into()],
            images: vec!["myapp:1".into()],
        }
    }

    #[test]
    fn test_resume_step_label_single_and_stack() {
        // 单镜像 1..5:打标签/导出压缩/上传镜像/同步文件/服务器部署
        assert_eq!(resume_step_label(MODE_SINGLE, 1), "打标签");
        assert_eq!(resume_step_label(MODE_SINGLE, 2), "导出压缩");
        assert_eq!(resume_step_label(MODE_SINGLE, 3), "上传镜像");
        assert_eq!(resume_step_label(MODE_SINGLE, 4), "同步文件");
        assert_eq!(resume_step_label(MODE_SINGLE, 5), "服务器部署");
        // 整栈 1..6:分类确认/打包/上传/装载/拉取/启动
        assert_eq!(resume_step_label(MODE_STACK, 1), "分类确认");
        assert_eq!(resume_step_label(MODE_STACK, 2), "打包");
        assert_eq!(resume_step_label(MODE_STACK, 3), "上传");
        assert_eq!(resume_step_label(MODE_STACK, 4), "装载");
        assert_eq!(resume_step_label(MODE_STACK, 5), "拉取");
        assert_eq!(resume_step_label(MODE_STACK, 6), "启动");
        // 越界(成功后即清除断点,理论不可达)与未知模式兜底
        assert_eq!(resume_step_label(MODE_SINGLE, 6), "部署收尾");
        assert_eq!(resume_step_label(MODE_SINGLE, 0), "部署收尾");
        assert_eq!(resume_step_label("rollback", 1), "部署收尾");
    }

    #[test]
    fn test_single_resume_artifacts_serde_roundtrip() {
        // camelCase 序列化(存入 resume-deploy.json 的 artifacts 字段)→ 读回逐字段相等
        let art = single_art();
        let v = serde_json::to_value(&art).unwrap();
        assert_eq!(v["originRef"], "myapp:latest");
        assert_eq!(v["useDateTag"], true);
        assert_eq!(v["tarLocal"], "C:\\Temp\\abc.tar.gz");
        assert_eq!(v["tarName"], "abc.tar.gz");
        let back: SingleResumeArtifacts = serde_json::from_value(v).unwrap();
        assert_eq!(back, art);
    }

    #[test]
    fn test_stack_resume_artifacts_serde_roundtrip() {
        let art = stack_art();
        let v = serde_json::to_value(&art).unwrap();
        assert_eq!(v["releaseTs"], "20260906-101010");
        assert_eq!(v["skipUnchanged"], true);
        assert_eq!(v["forceArchive"], false);
        assert_eq!(v["files"][0], "a.tar.gz");
        assert_eq!(v["images"][0], "myapp:1");
        // services 内嵌 StackServiceChoice(snake_case 契约不变)
        assert_eq!(v["services"][0]["service"], "web");
        assert_eq!(v["services"][0]["mode"], "Local");
        let back: StackResumeArtifacts = serde_json::from_value(v).unwrap();
        assert_eq!(back, art);
    }

    #[test]
    fn test_parse_resume_artifacts_corrupt_is_none() {
        // 非 JSON 对象(数组/字符串)→ None,调用方按断点数据损坏报错
        assert!(parse_single_artifacts(&serde_json::json!([1, 2, 3])).is_none());
        assert!(parse_stack_artifacts(&serde_json::json!("垃圾")).is_none());
        // JSON 对象但字段全缺 → serde default 宽松解析为默认值;关键产物缺失
        // 由 resume_context_of 的字段级校验拦截(步骤号 > 产物完成度 → 报错)
        let lenient = parse_single_artifacts(&serde_json::json!({"nope": 1}));
        assert!(lenient.is_some());
        let cp = ResumeCheckpoint {
            key: checkpoint_key("s1", "p1", MODE_SINGLE),
            mode: MODE_SINGLE.into(),
            step_next: 2,
            ts: String::new(),
            server_id: "s1".into(),
            project_id: "p1".into(),
            server_name: "s".into(),
            project_name: "p".into(),
            artifacts: serde_json::json!({"nope": 1}),
        };
        assert!(resume_context_of(&cp).is_err(), "步骤 2 但缺镜像引用应判损坏");
        // 合法数据 → Some
        assert!(parse_single_artifacts(&serde_json::to_value(single_art()).unwrap()).is_some());
        assert!(parse_stack_artifacts(&serde_json::to_value(stack_art()).unwrap()).is_some());
    }

    #[test]
    fn test_resume_local_tars_by_mode() {
        // 单镜像:tar_local;整栈:locals 全部;未知模式:空
        let single_cp = ResumeCheckpoint {
            key: checkpoint_key("s1", "p1", MODE_SINGLE),
            mode: MODE_SINGLE.into(),
            step_next: 3,
            ts: "2026-09-06 10:00:00".into(),
            server_id: "s1".into(),
            project_id: "p1".into(),
            server_name: "服务器".into(),
            project_name: "项目".into(),
            artifacts: serde_json::to_value(single_art()).unwrap(),
        };
        let tars = resume_local_tars(&single_cp);
        assert_eq!(tars, vec![PathBuf::from("C:\\Temp\\abc.tar.gz")]);

        let stack_cp = ResumeCheckpoint {
            key: checkpoint_key("s1", "p1", MODE_STACK),
            mode: MODE_STACK.into(),
            step_next: 4,
            artifacts: serde_json::to_value(stack_art()).unwrap(),
            ..single_cp.clone()
        };
        let tars = resume_local_tars(&stack_cp);
        assert_eq!(tars, vec![PathBuf::from("C:\\Temp\\a.tar.gz")]);

        let unknown = ResumeCheckpoint { mode: "rollback".into(), ..stack_cp.clone() };
        assert!(resume_local_tars(&unknown).is_empty());
        // artifacts 损坏 → 空(清理尽力而为,不让放弃操作失败)
        let corrupt = ResumeCheckpoint {
            artifacts: serde_json::json!("垃圾"),
            ..stack_cp
        };
        assert!(resume_local_tars(&corrupt).is_empty());
    }

    #[test]
    fn test_resume_context_of_validates() {
        let base = ResumeCheckpoint {
            key: checkpoint_key("s1", "p1", MODE_SINGLE),
            mode: MODE_SINGLE.into(),
            step_next: 3,
            ts: String::new(),
            server_id: "s1".into(),
            project_id: "p1".into(),
            server_name: "s".into(),
            project_name: "p".into(),
            artifacts: serde_json::to_value(single_art()).unwrap(),
        };
        // 合法单镜像断点
        let ctx = resume_context_of(&base).unwrap();
        assert_eq!(ctx.step_next, 3);
        assert_eq!(ctx.key, base.key);
        assert_eq!(ctx.single.tar_name.as_deref(), Some("abc.tar.gz"));

        // 未知模式 → 报错
        let bad_mode = ResumeCheckpoint { mode: "x".into(), ..base.clone() };
        assert!(resume_context_of(&bad_mode).is_err());

        // 步骤号越界 → 报错
        let bad_step = ResumeCheckpoint { step_next: 6, ..base.clone() };
        assert!(resume_context_of(&bad_step).is_err());

        // 步骤 1 未完成(image_ref 缺失)合法;步骤 3 但缺镜像引用 → 报错
        let mut art = single_art();
        art.image_ref = None;
        let step1 = ResumeCheckpoint { step_next: 1, artifacts: serde_json::to_value(&art).unwrap(), ..base.clone() };
        assert!(resume_context_of(&step1).is_ok());
        let step3 = ResumeCheckpoint { step_next: 3, artifacts: serde_json::to_value(&art).unwrap(), ..base.clone() };
        assert!(resume_context_of(&step3).is_err());

        // 整栈:合法 + 产物列表不一致 → 报错
        let stack_base = ResumeCheckpoint {
            key: checkpoint_key("s1", "p1", MODE_STACK),
            mode: MODE_STACK.into(),
            step_next: 4,
            artifacts: serde_json::to_value(stack_art()).unwrap(),
            ..base
        };
        assert!(resume_context_of(&stack_base).is_ok());
        let mut art = stack_art();
        art.locals.clear();
        let mismatch = ResumeCheckpoint { artifacts: serde_json::to_value(&art).unwrap(), ..stack_base };
        assert!(resume_context_of(&mismatch).is_err());
    }

    #[test]
    fn test_resume_view_of_mapping() {
        let cp = ResumeCheckpoint {
            key: checkpoint_key("s1", "p1", MODE_STACK),
            mode: MODE_STACK.into(),
            step_next: 3,
            ts: "2026-09-06 10:00:00".into(),
            server_id: "s1".into(),
            project_id: "p1".into(),
            server_name: "生产".into(),
            project_name: "博客".into(),
            artifacts: serde_json::json!({}),
        };
        let view = resume_view_of(&cp);
        assert_eq!(view.key, "s1|p1|stack");
        assert_eq!(view.mode, "stack");
        assert_eq!(view.step_next, 3);
        assert_eq!(view.step_label, "上传");
        assert_eq!(view.ts, "2026-09-06 10:00:00");
        assert_eq!(view.server_name, "生产");
        assert_eq!(view.project_name, "博客");
        // camelCase 序列化(前端契约)
        let v = serde_json::to_value(&view).unwrap();
        assert!(v.get("stepNext").is_some());
        assert!(v.get("stepLabel").is_some());
        assert!(v.get("serverName").is_some());
    }

    #[test]
    fn test_temp_file_guard_drop_semantics() {
        // new:Drop 删除;keep(断点活跃):Drop 保留(由成功收尾/放弃显式清理)
        let p1 = std::env::temp_dir().join(format!("dd-guard-{}.tmp", uuid::Uuid::new_v4()));
        std::fs::write(&p1, b"x").unwrap();
        {
            let _g = TempFileGuard::new(p1.clone());
        }
        assert!(!p1.exists(), "TempFileGuard::new 应在 Drop 时删除文件");

        let p2 = std::env::temp_dir().join(format!("dd-guard-{}.tmp", uuid::Uuid::new_v4()));
        std::fs::write(&p2, b"x").unwrap();
        {
            let _g = TempFileGuard::keep(p2.clone());
        }
        assert!(p2.exists(), "TempFileGuard::keep 不应在 Drop 时删除文件");
        std::fs::remove_file(&p2).ok();
    }

    #[test]
    fn test_checkpoint_trim_cleans_dropped_local_tars() {
        // 断点表超上限时被裁掉的条目,其断点期本地 tar 必须一并清理。
        // 不清理会静默泄漏临时盘:那些 tar 既不在断点表里(无法经「放弃断点」
        // 回收),也不属于任何失败台(界面上看不到)。批量部署 N 台各写一条
        // 断点,超过 MAX_CHECKPOINTS 时必然触发。
        let _guard = crate::config::TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-resume-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());

        // 建 MAX_CHECKPOINTS 条,每条带一个真实存在的本地 tar(ts 递增)
        let mut tars = Vec::new();
        for i in 0..crate::config::MAX_CHECKPOINTS {
            let tar = dir.join(format!("keep-{}.tar.gz", i));
            std::fs::write(&tar, b"x").unwrap();
            tars.push(tar.clone());
            let mut art = single_art();
            art.tar_local = Some(tar.to_string_lossy().to_string());
            let cp = ResumeCheckpoint {
                key: checkpoint_key(&format!("s{}", i), "p1", MODE_SINGLE),
                mode: MODE_SINGLE.into(),
                step_next: 3,
                ts: format!("2026-09-06 10:00:{:02}", i),
                server_id: format!("s{}", i),
                project_id: "p1".into(),
                server_name: "s".into(),
                project_name: "p".into(),
                artifacts: serde_json::to_value(&art).unwrap(),
            };
            crate::config::save_checkpoint(&cp).unwrap();
        }

        // 再写一条(超上限):最旧的 s0 被裁掉,其 tar 应被删除
        let overflow_tar = dir.join("overflow.tar.gz");
        std::fs::write(&overflow_tar, b"x").unwrap();
        let mut art = single_art();
        art.tar_local = Some(overflow_tar.to_string_lossy().to_string());
        let cp = ResumeCheckpoint {
            key: checkpoint_key("s99", "p1", MODE_SINGLE),
            mode: MODE_SINGLE.into(),
            step_next: 3,
            ts: "2026-09-06 11:00:00".into(),
            server_id: "s99".into(),
            project_id: "p1".into(),
            server_name: "s".into(),
            project_name: "p".into(),
            artifacts: serde_json::to_value(&art).unwrap(),
        };
        let dropped = crate::config::save_checkpoint(&cp).unwrap();

        // 返回值报告了被裁条目(config 层不解析 artifacts,交调用方清理)
        assert_eq!(dropped.len(), 1, "应只裁掉最旧的 1 条");
        assert_eq!(dropped[0].server_id, "s0");
        // 被裁条目的 tar 由 commands 层口径解析得到
        let dropped_tars = resume_local_tars(&dropped[0]);
        assert_eq!(dropped_tars, vec![tars[0].clone()]);
        // 履行清理(与实际调用点 checkpoint_save 同口径)
        for path in &dropped_tars {
            std::fs::remove_file(path).unwrap();
        }
        assert!(!tars[0].exists(), "被裁断点的本地 tar 应被清理");
        assert!(tars[1].exists(), "未超限的断点 tar 不应被误删");
        assert!(overflow_tar.exists(), "新写入断点的 tar 不应被删");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_checkpoint_cleanup_on_success_removes_tar_and_checkpoint() {
        // 成功收尾:断点删除 + 保留的本地 tar 删除;文件已不存在时静默
        let _guard = crate::config::TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ddtest-resume-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::env::set_var("DD_CONFIG_DIR", dir.to_str().unwrap());

        let key = checkpoint_key("s9", "p9", MODE_SINGLE);
        let cp = ResumeCheckpoint {
            key: key.clone(),
            mode: MODE_SINGLE.into(),
            step_next: 5,
            ts: "2026-09-06 10:00:00".into(),
            server_id: "s9".into(),
            project_id: "p9".into(),
            server_name: "s".into(),
            project_name: "p".into(),
            artifacts: serde_json::json!({}),
        };
        crate::config::save_checkpoint(&cp).unwrap();
        let tar = dir.join("left.tar.gz");
        std::fs::write(&tar, b"x").unwrap();

        checkpoint_cleanup_on_success(&key, &[tar.clone(), dir.join("already-gone.tar.gz")]);
        assert!(crate::config::load_resume_map().get(&key).is_none(), "断点应被清除");
        assert!(!tar.exists(), "保留的本地 tar 应被删除");

        std::fs::remove_dir_all(&dir).ok();
    }
