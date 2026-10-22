//! 设备树：用户层级 → 分组 → 设备。
//!
//! 数据隔离规则：
//!   - 管理员看到整片森林，外加「未分配」节点；
//!   - 普通用户只看到自己 + 全部下级用户拥有的设备，同级的其他分支完全不可见。
//!
//! 树在服务端一次性算好（含在线数与设备数），前端只负责渲染与折叠，
//! 避免把可见性判断散落到 JS 里造成越权。

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::Serialize;

use crate::storage::{DeviceRow, LatestRow, UserRow};

#[derive(Debug, Clone, Default, Serialize)]
pub struct DeviceNode {
    pub sn: String,
    pub mdvr_id: String,
    pub firmware: String,
    pub owner: String,
    pub group: String,
    pub online: bool,
    pub busy: bool,
    pub battery: i64,
    pub health: i64,
    pub last_seen_ms: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct GroupNode {
    pub key: String,
    pub name: String,
    pub owner: String,
    pub total: usize,
    pub online: usize,
    pub devices: Vec<DeviceNode>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct UserNode {
    pub key: String,
    /// 归属过滤用的用户名（未分配节点用占位名）
    pub name: String,
    /// 展示名，普通用户与 name 相同
    pub label: String,
    pub role: String,
    pub parent: String,
    pub depth: usize,
    pub total: usize,
    pub online: usize,
    /// 该用户直属设备按分组聚合
    pub groups: Vec<GroupNode>,
    /// 下级用户
    pub children: Vec<UserNode>,
}

impl UserNode {
    fn recount(&mut self) {
        let mut total = self.groups.iter().map(|g| g.total).sum::<usize>();
        let mut online = self.groups.iter().map(|g| g.online).sum::<usize>();
        for c in &self.children {
            total += c.total;
            online += c.online;
        }
        self.total = total;
        self.online = online;
    }
}

/// 用户自身 + 全部下级用户名。
pub fn subtree_usernames(users: &[UserRow], root: &str) -> HashSet<String> {
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    for u in users {
        children
            .entry(u.parent.as_str())
            .or_default()
            .push(u.username.as_str());
    }
    let mut out = HashSet::new();
    let mut stack = vec![root];
    while let Some(name) = stack.pop() {
        if !out.insert(name.to_string()) {
            continue;
        }
        if let Some(kids) = children.get(name) {
            stack.extend(kids.iter().copied());
        }
    }
    out
}

/// 把设备按「归属 → 分组」挂到用户节点上。
fn attach_devices(
    node: &mut UserNode,
    devices: &[DeviceRow],
    latest: &HashMap<String, LatestRow>,
    online: &HashMap<String, bool>,
    busy: &HashSet<String>,
) {
    let mut grouped: BTreeMap<&str, Vec<&DeviceRow>> = BTreeMap::new();
    for d in devices.iter().filter(|d| d.owner == node.name) {
        grouped.entry(d.group_name.as_str()).or_default().push(d);
    }
    for (group, mut items) in grouped {
        // 在线优先，其次按 SN
        items.sort_by(|a, b| {
            let ao = online.get(&a.sn).copied().unwrap_or(false);
            let bo = online.get(&b.sn).copied().unwrap_or(false);
            bo.cmp(&ao).then_with(|| a.sn.cmp(&b.sn))
        });
        let devices: Vec<DeviceNode> = items
            .iter()
            .map(|d| {
                let is_online = online.get(&d.sn).copied().unwrap_or(false);
                let live = latest.get(&d.sn);
                DeviceNode {
                    sn: d.sn.clone(),
                    mdvr_id: d.mdvr_id.clone(),
                    firmware: d.firmware_version.clone(),
                    owner: d.owner.clone(),
                    group: d.group_name.clone(),
                    online: is_online,
                    busy: busy.contains(&d.sn),
                    battery: live.map(|l| l.battery).unwrap_or(0),
                    health: live.map(|l| l.health).unwrap_or(0),
                    last_seen_ms: if is_online {
                        live.map(|l| l.ts_ms).unwrap_or(d.last_seen_ms)
                    } else {
                        d.last_seen_ms
                    },
                }
            })
            .collect();
        let total = devices.len();
        let on = devices.iter().filter(|d| d.online).count();
        node.groups.push(GroupNode {
            key: format!("g:{}:{}", node.name, group),
            name: group.to_string(),
            owner: node.name.clone(),
            total,
            online: on,
            devices,
        });
    }
}

/// 构建用户子树。`root` 为当前登录用户，管理员会拿到整片森林。
pub fn build_tree(
    users: &[UserRow],
    devices: &[DeviceRow],
    latest: &HashMap<String, LatestRow>,
    online: &HashMap<String, bool>,
    busy: &HashSet<String>,
    root: &str,
    is_admin: bool,
) -> Vec<UserNode> {
    let known: HashSet<&str> = users.iter().map(|u| u.username.as_str()).collect();
    let mut children: HashMap<&str, Vec<&UserRow>> = HashMap::new();
    for u in users {
        // 上级不存在（被删或写错）时按顶级处理，避免整棵子树消失
        let parent = if u.parent.is_empty() || !known.contains(u.parent.as_str()) {
            ""
        } else {
            u.parent.as_str()
        };
        children.entry(parent).or_default().push(u);
    }
    for kids in children.values_mut() {
        kids.sort_by(|a, b| a.username.cmp(&b.username));
    }

    fn make(
        u: &UserRow,
        depth: usize,
        children: &HashMap<&str, Vec<&UserRow>>,
        devices: &[DeviceRow],
        latest: &HashMap<String, LatestRow>,
        online: &HashMap<String, bool>,
        busy: &HashSet<String>,
    ) -> UserNode {
        let mut node = UserNode {
            key: format!("u:{}", u.username),
            name: u.username.clone(),
            label: u.username.clone(),
            role: u.role.clone(),
            parent: u.parent.clone(),
            depth,
            ..Default::default()
        };
        attach_devices(&mut node, devices, latest, online, busy);
        if let Some(kids) = children.get(u.username.as_str()) {
            node.children = kids
                .iter()
                .map(|k| make(k, depth + 1, children, devices, latest, online, busy))
                .collect();
        }
        node.recount();
        node
    }

    let roots: Vec<&UserRow> = if is_admin {
        children.get("").cloned().unwrap_or_default()
    } else {
        users
            .iter()
            .filter(|u| u.username == root)
            .collect::<Vec<_>>()
    };

    let mut forest: Vec<UserNode> = roots
        .into_iter()
        .map(|u| make(u, 0, &children, devices, latest, online, busy))
        .collect();

    // 未分配设备只对管理员可见，作为管理员节点下的一个子节点，保证计数自洽
    if is_admin {
        let orphans: Vec<&DeviceRow> = devices
            .iter()
            .filter(|d| d.owner.is_empty() || !known.contains(d.owner.as_str()))
            .collect();
        if !orphans.is_empty() {
            // 统一改写成伪归属，再一次性分组，避免同名分组被拆成多份
            let orphans: Vec<DeviceRow> = orphans
                .into_iter()
                .map(|d| DeviceRow {
                    owner: "__unassigned__".into(),
                    ..d.clone()
                })
                .collect();
            let mut node = UserNode {
                key: "u:__unassigned__".into(),
                name: "__unassigned__".into(),
                label: "未分配设备".into(),
                role: "—".into(),
                depth: 1,
                ..Default::default()
            };
            attach_devices(&mut node, &orphans, latest, online, busy);
            node.recount();
            match forest.iter_mut().find(|n| n.name == root) {
                Some(root_node) => {
                    root_node.children.push(node);
                    root_node.recount();
                }
                None => forest.push(node),
            }
        }
    }

    forest
}

/// 当前用户可见的设备 SN 集合，用于详情/指令/OTA 的越权校验。
pub fn visible_sns(tree: &[UserNode], is_admin: bool) -> HashSet<String> {
    fn walk(node: &UserNode, out: &mut HashSet<String>) {
        for g in &node.groups {
            for d in &g.devices {
                out.insert(d.sn.clone());
            }
        }
        for c in &node.children {
            walk(c, out);
        }
    }
    let mut out = HashSet::new();
    for n in tree {
        walk(n, &mut out);
    }
    let _ = is_admin;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(name: &str, role: &str, parent: &str) -> UserRow {
        UserRow {
            username: name.into(),
            role: role.into(),
            created_at_ms: 0,
            parent: parent.into(),
        }
    }

    fn device(sn: &str, owner: &str, group: &str) -> DeviceRow {
        DeviceRow {
            sn: sn.into(),
            owner: owner.into(),
            group_name: group.into(),
            ..Default::default()
        }
    }

    fn forest() -> Vec<UserRow> {
        vec![
            user("admin", "admin", ""),
            user("fleet_a", "operator", "admin"),
            user("driver_1", "viewer", "fleet_a"),
            user("fleet_b", "operator", ""),
        ]
    }

    #[test]
    fn subtree_includes_descendants_only() {
        let users = forest();
        let a = subtree_usernames(&users, "fleet_a");
        assert!(a.contains("fleet_a") && a.contains("driver_1"));
        assert!(!a.contains("fleet_b") && !a.contains("admin"));
    }

    #[test]
    fn non_admin_sees_only_own_branch() {
        let users = forest();
        let devices = vec![
            device("UPS-1", "fleet_a", "华东"),
            device("UPS-2", "driver_1", "默认分组"),
            device("UPS-3", "fleet_b", "默认分组"),
        ];
        let tree = build_tree(
            &users,
            &devices,
            &HashMap::new(),
            &HashMap::new(),
            &HashSet::new(),
            "fleet_a",
            false,
        );
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].total, 2);
        assert_eq!(tree[0].children.len(), 1);
        assert_eq!(tree[0].children[0].total, 1);
        let sns = visible_sns(&tree, false);
        assert!(sns.contains("UPS-1") && sns.contains("UPS-2"));
        assert!(!sns.contains("UPS-3"));
    }

    #[test]
    fn admin_sees_forest_and_unassigned() {
        let users = forest();
        let devices = vec![
            device("UPS-1", "fleet_a", "华东"),
            device("UPS-3", "fleet_b", "默认分组"),
            device("UPS-9", "", "默认分组"),
        ];
        let tree = build_tree(
            &users,
            &devices,
            &HashMap::new(),
            &HashMap::new(),
            &HashSet::new(),
            "admin",
            true,
        );
        // 顶层仍有 admin 与 fleet_b（历史脏数据），未分配节点挂在 admin 下
        assert_eq!(tree.len(), 2);
        let admin = &tree[0];
        assert_eq!(admin.name, "admin");
        // UPS-1（fleet_a）+ UPS-9（未分配）
        assert_eq!(admin.total, 2);
        assert!(
            admin
                .children
                .iter()
                .any(|c| c.label == "未分配设备" && c.total == 1)
        );
    }

    #[test]
    fn admin_root_counts_unassigned() {
        // 归一化之后：只有 admin 是顶级，其余都在 admin 之下
        let users = vec![
            user("admin", "admin", ""),
            user("fleet_a", "operator", "admin"),
            user("driver_1", "viewer", "fleet_a"),
            user("fleet_b", "operator", "admin"),
        ];
        let devices = vec![
            device("UPS-1", "fleet_a", "华东"),
            device("UPS-2", "driver_1", "默认分组"),
            device("UPS-3", "fleet_b", "华北"),
            device("UPS-9", "", "默认分组"),
        ];
        let tree = build_tree(
            &users,
            &devices,
            &HashMap::new(),
            &HashMap::new(),
            &HashSet::new(),
            "admin",
            true,
        );
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].name, "admin");
        // 4 台设备全部计入 admin 子树
        assert_eq!(tree[0].total, 4);
        let sns = visible_sns(&tree, true);
        assert_eq!(sns.len(), 4);
    }

    #[test]
    fn online_count_rolls_up() {
        let users = forest();
        let devices = vec![device("UPS-1", "fleet_a", "g"), device("UPS-2", "driver_1", "g")];
        let mut online = HashMap::new();
        online.insert("UPS-2".to_string(), true);
        let tree = build_tree(
            &users,
            &devices,
            &HashMap::new(),
            &online,
            &HashSet::new(),
            "admin",
            true,
        );
        assert_eq!(tree[0].total, 2);
        assert_eq!(tree[0].online, 1);
    }
}
