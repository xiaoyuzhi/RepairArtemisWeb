// ============================================================================
// nginx — nginx 配置只读解析 (spec §7)。不修改、不 reload、不递归展开 include。
// ============================================================================

use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq)]
pub struct Upstream {
    pub name: String,
    pub host: String,
    pub port: u16,
}

#[derive(Debug, PartialEq, Clone)]
pub enum RouteTarget {
    Upstream(String),
    /// proxy_pass 目标含变量, 静态不可判定
    Dynamic,
    None,
}

#[derive(Debug, PartialEq, Clone)]
pub struct Route {
    pub path: String,
    pub target: RouteTarget,
}

#[derive(Debug, PartialEq)]
pub struct ArtemisMode {
    pub value: String,
    pub loc: String,
}

#[derive(Debug, Default)]
pub struct NginxInfo {
    pub root: PathBuf,
    pub upstreams: Vec<Upstream>,
    pub routes: Vec<Route>,
    pub artemis_mode: Option<String>,
    pub artemis_mode_loc: Option<String>,
}

impl NginxInfo {
    /// `$artemis != "local"` 时 /artemis* 落到 https_artemis_remote (127.0.0.1:443), 即 nginx 自身。
    pub fn loopback_risk(&self) -> bool {
        self.artemis_mode.as_deref().map(|m| m != "local").unwrap_or(false)
            && self.upstreams.iter().any(|u| u.name == "https_artemis_remote" && u.port == 443)
    }
}

fn strip_comment(line: &str) -> &str {
    match line.find('#') {
        Some(i) => &line[..i],
        None => line,
    }
}

pub fn parse_upstreams(text: &str) -> Vec<Upstream> {
    let mut out = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let t = strip_comment(lines[i]).trim().to_string();
        i += 1;
        let Some(rest) = t.strip_prefix("upstream") else { continue };
        if rest.chars().next().map(|c| c.is_alphanumeric()).unwrap_or(false) {
            continue;
        }
        let rest = rest.trim_start();
        let name: String = rest.chars().take_while(|c| *c != '{' && !c.is_whitespace()).collect();
        if name.is_empty() {
            continue;
        }
        let mut depth = t.matches('{').count() as i32 - t.matches('}').count() as i32;
        let mut body = String::from(rest);
        while i < lines.len() && depth > 0 {
            let c = strip_comment(lines[i]);
            depth += c.matches('{').count() as i32 - c.matches('}').count() as i32;
            body.push('\n');
            body.push_str(c);
            i += 1;
        }
        for bl in body.lines() {
            let bt = bl.trim();
            if let Some(args) = bt.strip_prefix("server") {
                let a = args.trim().trim_end_matches(';').trim().to_string();
                if let Some((h, p)) = a.rsplit_once(':') {
                    if let Ok(port) = p.trim().parse::<u16>() {
                        out.push(Upstream {
                            name: name.clone(),
                            host: h.trim().to_string(),
                            port,
                        });
                        break;
                    }
                }
            }
        }
    }
    out
}

/// 花括号配对计数提取 location 块 (Review Focus #3)。
pub fn parse_locations(text: &str) -> Vec<Route> {
    let mut out = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let t = strip_comment(lines[i]).trim().to_string();
        i += 1;
        let Some(rest) = t.strip_prefix("location") else { continue };
        if rest.chars().next().map(|c| c.is_alphanumeric()).unwrap_or(false) {
            continue;
        }
        let mut path = String::new();
        for tk in rest.split_whitespace() {
            if tk == "^~" || tk == "~" || tk == "~*" || tk == "=" || tk.starts_with('~') {
                continue; // 修饰符
            }
            path = tk.trim_end_matches('{').to_string();
            break;
        }
        if path.is_empty() {
            continue;
        }
        let mut depth = t.matches('{').count() as i32 - t.matches('}').count() as i32;
        let mut body = String::new();
        while i < lines.len() && depth > 0 {
            let c = strip_comment(lines[i]);
            depth += c.matches('{').count() as i32 - c.matches('}').count() as i32;
            body.push_str(c);
            body.push('\n');
            i += 1;
        }
        // 取 local 分支的 proxy_pass: 首个字面 http(s):// 目标
        let mut target = RouteTarget::None;
        for bl in body.lines() {
            let bt = bl.trim();
            if let Some(args) = bt.strip_prefix("proxy_pass") {
                let a = args.trim().trim_end_matches(';').trim().to_string();
                if a.contains('$') {
                    target = RouteTarget::Dynamic;
                    break;
                }
                if let Some(name) = a.rsplit('/').next() {
                    if !name.is_empty() {
                        target = RouteTarget::Upstream(name.to_string());
                        break;
                    }
                }
            }
        }
        out.push(Route { path, target });
    }
    out
}

pub fn find_artemis_mode(text: &str, file: &str) -> Option<ArtemisMode> {
    for (n, line) in text.lines().enumerate() {
        let t = strip_comment(line).trim();
        let Some(rest) = t.strip_prefix("set") else { continue };
        let rest = rest.trim_start();
        if let Some((var, val)) = rest.split_once(char::is_whitespace) {
            if var == "$artemis" {
                let v = val.trim().trim_end_matches(';').trim().trim_matches('"').to_string();
                return Some(ArtemisMode { value: v, loc: format!("{}:{}", file, n + 1) });
            }
        }
    }
    None
}

fn confs_in(dir: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file() && p.extension().map(|x| x == "conf").unwrap_or(false) {
                v.push(p);
            }
        }
    }
    v.sort();
    v
}

/// root 为 nginx 根目录 (含 conf/)。spec §7.3: 只扫 conf/、conf/Mode/、../ssl/ 三处。
pub fn load_nginx_info(root: &Path) -> NginxInfo {
    let mut info = NginxInfo { root: root.to_path_buf(), ..Default::default() };
    let conf = root.join("conf");
    let mut files: Vec<PathBuf> = Vec::new();
    files.extend(confs_in(&conf));
    files.extend(confs_in(&conf.join("Mode")));
    files.extend(confs_in(&root.join("..").join("ssl")));
    if let Ok(main) = std::fs::read_to_string(conf.join("nginx.conf")) {
        info.upstreams = parse_upstreams(&main);
        info.routes = parse_locations(&main);
    }
    for f in &files {
        let Ok(text) = std::fs::read_to_string(f) else { continue };
        for r in parse_locations(&text) {
            if !info.routes.iter().any(|e| e.path == r.path) {
                info.routes.push(r);
            }
        }
        if info.artemis_mode.is_none() {
            let rel = f.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
            if let Some(m) = find_artemis_mode(&text, &rel) {
                info.artemis_mode = Some(m.value);
                info.artemis_mode_loc = Some(m.loc);
            }
        }
    }
    info
}

/// 路由 -> 目标端口。remote 模式下 local 分支失效, 落到 remote upstream。
pub fn resolve_route_port(info: &NginxInfo, path: &str) -> Option<u16> {
    let r = info.routes.iter().find(|r| r.path == path)?;
    let name = match &r.target {
        RouteTarget::Upstream(n) => n.clone(),
        RouteTarget::Dynamic | RouteTarget::None => return None,
    };
    if info.loopback_risk() {
        return info.upstreams.iter().find(|u| u.name == "https_artemis_remote").map(|u| u.port);
    }
    info.upstreams.iter().find(|u| u.name == name).map(|u| u.port)
}

/// 在 base 下查找含 conf/nginx.conf 的目录, 返回该目录 (nginx 根)。
pub fn locate_nginx_conf(base: &Path) -> Option<PathBuf> {
    let mut stack = vec![base.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            if p.join("conf").join("nginx.conf").is_file() {
                return Some(p);
            }
            stack.push(p);
        }
    }
    None
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::model::fixture_path;
    use std::fs;

    fn root(name: &str) -> PathBuf {
        fixture_path("").join(name)
    }

    /// 真实布局里 ssl/ 与 nginx/ 同级 (nginx.conf 用 `include ../../ssl/*.conf` 指向它),
    /// 所以夹具按 `<name>/nginx/conf` + `<name>/ssl` 摆放, 传给解析器的是 `<name>/nginx`。
    fn nginx_root(name: &str) -> PathBuf {
        root(name).join("nginx")
    }

    #[test]
    fn 提取upstream端口() {
        let text =
            fs::read_to_string(nginx_root("nginx-local").join("conf").join("nginx.conf")).unwrap();
        let ups = parse_upstreams(&text);
        let find = |n: &str| ups.iter().find(|u| u.name == n).map(|u| u.port);
        assert_eq!(find("http_artemis"), Some(9016));
        assert_eq!(find("http_artemis_web"), Some(9017));
        assert_eq!(find("http_artemis_portal"), Some(9018));
        assert_eq!(find("https_artemis_remote"), Some(443));
    }

    #[test]
    fn 嵌套花括号不得提前结束location块() {
        // Review Focus #3: 块内 if (...) { } 会让朴素"找下一个 }"提前收尾
        let text = fs::read_to_string(
            nginx_root("nginx-local").join("conf").join("Mode").join("nginx_artemis.conf"),
        )
        .unwrap();
        let routes = parse_locations(&text);
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].path, "/artemis");
        assert_eq!(routes[1].path, "/artemis-web");
        // 归因取 local 分支的 upstream, 不是 remote 那条
        assert_eq!(routes[1].target, RouteTarget::Upstream("http_artemis_web".into()));
    }

    #[test]
    fn 变量型proxy_pass标记为动态不报错() {
        let routes =
            parse_locations("location /a {\n proxy_pass $scheme://$cookie_x;\n}\n");
        assert_eq!(routes[0].target, RouteTarget::Dynamic);
    }

    #[test]
    fn 读取artemis_mode并给出文件行号() {
        let p = root("nginx-local").join("ssl").join("artemis.conf");
        let m = find_artemis_mode(&fs::read_to_string(&p).unwrap(), "artemis.conf").unwrap();
        assert_eq!(m.value, "local");
        assert_eq!(m.loc, "artemis.conf:1");
        let p2 = root("nginx-remote").join("ssl").join("artemis.conf");
        assert_eq!(
            find_artemis_mode(&fs::read_to_string(&p2).unwrap(), "x").unwrap().value,
            "remote"
        );
    }

    #[test]
    fn 整仓加载后能解析出三条路由的目标端口() {
        let info = load_nginx_info(&nginx_root("nginx-local"));
        assert_eq!(info.artemis_mode.as_deref(), Some("local"));
        assert_eq!(resolve_route_port(&info, "/artemis-web"), Some(9017));
        assert_eq!(resolve_route_port(&info, "/artemis-portal"), Some(9018));
        assert_eq!(resolve_route_port(&info, "/artemis"), Some(9016));
        assert_eq!(resolve_route_port(&info, "/nope"), None);
    }

    #[test]
    fn remote模式下明确报回环() {
        let info = load_nginx_info(&nginx_root("nginx-remote"));
        assert_eq!(info.artemis_mode.as_deref(), Some("remote"));
        // local 分支失效 -> 落到 https_artemis_remote = 127.0.0.1:443 = nginx 自身
        assert_eq!(resolve_route_port(&info, "/artemis-web"), Some(443));
        assert!(info.loopback_risk(), "remote 模式必须被识别为回环");
    }

    #[test]
    fn 从平台根目录能定位到nginx根() {
        // Task 9 依赖此函数; 真实布局为 VSM Servers/Web Service/nginx/conf/nginx.conf
        let base = fixture_path("").join("nginx-local");
        assert_eq!(locate_nginx_conf(&base), Some(nginx_root("nginx-local")));
        // 无 nginx 的目录树必须返回 None 而不是 panic
        let empty = std::env::temp_dir().join(format!("raw_t4_{}", std::process::id()));
        let _ = fs::remove_dir_all(&empty);
        fs::create_dir_all(&empty).unwrap();
        assert_eq!(locate_nginx_conf(&empty), None);
        let _ = fs::remove_dir_all(&empty);
    }
}
