#!/bin/bash
# 用户管理黑盒回归矩阵（手动运行，不由 cargo test 触发）
#
# 用法：
#   1) 起一个临时实例，避免污染生产库：
#        UPS_DB_PATH=D:/tmp_ut/t.duckdb UPS_INGEST_ADDR=127.0.0.1:1690 \
#        UPS_WEB_ADDR=127.0.0.1:18090 ./target/release/mdvr_ups.exe &
#   2) bash tests/user_admin_matrix.sh
#   3) 结束后删掉 D:/tmp_ut
#
# 覆盖：创建校验 / 重复创建 / 重置密码 / 层级调整 / 删除保护 /
#       设备归属 / 非管理员越权（RBAC 端到端，含真实模拟器设备）
#
# 响应约定（写断言时容易踩坑）：
#   - 表单校验失败        -> HTTP 200 + <div class="toast err">MSG</div>
#   - 用户管理 POST 成功  -> HTTP 303 + Location: /admin/users?msg=MSG
#   - 设备归属 POST       -> HTTP 200 + <div class="toast ok">MSG</div>（HTMX 行内表单）
#   - 登录是否成功        -> 看 /api/tree：200=已登录，401=未登录
#   - 每轮跑前务必换一个空库，否则上一轮建的用户会让后续断言全部错位

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
A=http://127.0.0.1:18090
H=/tmp/ut_h.txt; B=/tmp/ut_b.txt; UP=/tmp/ut_users.html
J=/tmp/ut_admin.txt
pass=0; fail=0

# req <jar> <path> <data>  ->  "HTTP_CODE<TAB>FLASH_MSG"
req() {
  curl -s --noproxy '*' -b "$1" -D $H -o $B -X POST "$A$2" ${3:+-d "$3"}
  local c loc f
  c=$(awk 'NR==1{print $2}' $H)
  loc=$(grep -i "^location:" $H | sed 's/.*msg=//;s/\r//')
  if [ -n "$loc" ]; then
    f=$(python -c "import sys,urllib.parse;print(urllib.parse.unquote(sys.argv[1]))" "$loc")
  else
    f=$(sed -n 's/.*<div class="toast [a-z]*">\(.*\)<\/div>.*/\1/p' $B | head -1)
  fi
  printf '%s\t%s' "$c" "$f"
}
chk()  { if [ "$3" = "$2" ]; then pass=$((pass+1)); echo "  PASS  $1"; else fail=$((fail+1)); echo "  FAIL  $1: 期望 $2 实际 $3"; fi; }
chkc() { if echo "$3" | grep -q "$2"; then pass=$((pass+1)); echo "  PASS  $1"; else fail=$((fail+1)); echo "  FAIL  $1: 期望含「$2」实际「$(echo "$3"|head -c 70)」"; fi; }
chkr() { # <jar> <path> <data> <name> <expected_code>
  local out c f
  out=$(req "$1" "$2" "$3"); c=${out%%$'	'*}; f=${out#*$'	'}
  if [ "$c" = "$5" ]; then pass=$((pass+1)); echo "  PASS  $4"; else fail=$((fail+1)); echo "  FAIL  $4: 期望 $5 实际 $c 「$f」"; fi; }
sc()   { req "$1" "$2" "$3" | cut -f1; }
sf()   { req "$1" "$2" "$3" | cut -f2; }
getc() { curl -s --noproxy '*' -b "$1" -o /dev/null -w '%{http_code}' "$A$2"; }
users_page() { curl -s --noproxy '*' -b "$1" $A/admin/users -o $UP; }
roleof()   { awk -v u="$1" 'BEGIN{RS="<tr>"} $0 ~ ("<strong>" u "</strong>")' $UP | grep -o 'role-badge">[a-z_]*' | head -1 | sed 's/.*">//'; }
parentof() { awk -v u="$1" 'BEGIN{RS="<tr>"} $0 ~ ("<strong>" u "</strong>")' $UP | grep -o '<span class="mono">[a-z_]*' | head -1 | sed 's/.*">//'; }

curl -s --noproxy '*' -o /dev/null -c $J -X POST $A/login -d "username=admin&password=admin123&next=/admin/users"
echo "管理员会话: $(getc $J /api/tree) (期望 200)"

echo "== 1. 创建校验 =="
chkc "用户名过短"   "用户名至少 3 个字符" "$(sf $J /admin/users 'username=ab&password=123456&role=viewer&parent=')"
chkc "用户名含空格" "只能包含字母"        "$(sf $J /admin/users 'username=a%20b%20c&password=123456&role=viewer&parent=')"
chkc "用户名含斜杠" "只能包含字母"        "$(sf $J /admin/users 'username=a/b/c&password=123456&role=viewer&parent=')"
chkc "密码过短"     "密码至少 6 位"       "$(sf $J /admin/users 'username=alice&password=12345&role=viewer&parent=')"
chkc "密码全空白"   "密码至少 6 位"       "$(sf $J /admin/users 'username=alice&password=%20%20%20%20%20%20&role=viewer&parent=')"
chkc "非法角色"     "非法角色"            "$(sf $J /admin/users 'username=alice&password=123456&role=superuser&parent=')"
chkc "上级不存在"   "上级用户不存在"      "$(sf $J /admin/users 'username=alice&password=123456&role=viewer&parent=ghost')"
chkc "上级是自己"   "上级不能是用户自己"  "$(sf $J /admin/users 'username=alice&password=123456&role=viewer&parent=alice')"
users_page $J; chkc "校验失败不建号" "0" "$(grep -c '<strong>alice</strong>' $UP)"

echo "== 2. 正常创建 =="
chk "alice 创建(viewer, 挂根)"    303 "$(sc $J /admin/users 'username=alice&password=alice123&role=viewer&parent=')"
chk "bob 创建(operator, 挂 alice)" 303 "$(sc $J /admin/users 'username=bob&password=bob12345&role=operator&parent=alice')"
chk "carol 创建(viewer, 挂 bob)"   303 "$(sc $J /admin/users 'username=carol&password=carol123&role=viewer&parent=bob')"
chk "dave 创建(admin, 挂 alice)"   303 "$(sc $J /admin/users 'username=dave&password=dave12345&role=admin&parent=alice')"
chk "erin 创建(operator, 挂 bob)"  303 "$(sc $J /admin/users 'username=erin&password=erin12345&role=operator&parent=bob')"
chk "fred 创建(viewer, 挂 bob)"    303 "$(sc $J /admin/users 'username=fred&password=fred12345&role=viewer&parent=alice')"
users_page $J
chk "alice 角色=viewer"   "viewer"   "$(roleof alice)"
chk "bob 角色=operator"   "operator" "$(roleof bob)"
chk "bob 上级=alice"      "alice"    "$(parentof bob)"
chk "carol 上级=bob"      "bob"      "$(parentof carol)"
chk "dave 角色=admin"     "admin"    "$(roleof dave)"
chk "erin 角色=operator"  "operator" "$(roleof erin)"
chk "fred 角色=viewer"    "viewer"   "$(roleof fred)"

echo "== 3. 重复创建必须被拒（修复前会覆盖口令与角色）=="
chkc "重复 alice 被拒" "用户名已存在" "$(sf $J /admin/users 'username=alice&password=hacked1&role=admin&parent=alice')"
rm -f /tmp/ut_x.txt
curl -s --noproxy '*' -o /dev/null -c /tmp/ut_x.txt -X POST $A/login -d "username=alice&password=alice123&next=/monitor"
chk "alice 原密码仍可登录" 200 "$(getc /tmp/ut_x.txt /api/tree)"
rm -f /tmp/ut_x2.txt
curl -s --noproxy '*' -o /dev/null -c /tmp/ut_x2.txt -X POST $A/login -d "username=alice&password=hacked1&next=/monitor"
chk "覆盖口令未生效(401)" 401 "$(getc /tmp/ut_x2.txt /api/tree)"
users_page $J; chk "覆盖角色未生效" "viewer" "$(roleof alice)"

echo "== 4. 重置密码 =="
chkc "密码过短被拒"   "密码至少 6 位" "$(sf $J /admin/users/password 'username=alice&password=123')"
chkc "密码全空白被拒" "密码至少 6 位" "$(sf $J /admin/users/password 'username=alice&password=%20%20%20%20%20%20')"
chkc "用户不存在被拒" "用户不存在"   "$(sf $J /admin/users/password 'username=ghost&password=abcdef')"
chkc "重置成功"       "密码已重置"   "$(sf $J /admin/users/password 'username=alice&password=newpass1')"
rm -f /tmp/ut_y.txt
curl -s --noproxy '*' -o /dev/null -c /tmp/ut_y.txt -X POST $A/login -d "username=alice&password=newpass1&next=/monitor"
chk "新密码可登录" 200 "$(getc /tmp/ut_y.txt /api/tree)"
rm -f /tmp/ut_z.txt
curl -s --noproxy '*' -o /dev/null -c /tmp/ut_z.txt -X POST $A/login -d "username=alice&password=alice123&next=/monitor"
chk "旧密码已失效(401)" 401 "$(getc /tmp/ut_z.txt /api/tree)"
users_page $J
chk "重置不改角色" "viewer" "$(roleof alice)"
chk "重置不改层级" "admin"  "$(parentof alice)"

echo "== 5. 层级调整 =="
chkc "上级不存在被拒" "上级用户不存在"       "$(sf $J /admin/users/parent 'username=bob&parent=ghost')"
chkc "目标用户不存在" "用户不存在"           "$(sf $J /admin/users/parent 'username=ghost&parent=alice')"
chkc "不能挂到自己下" "上级不能是用户自己"   "$(sf $J /admin/users/parent 'username=bob&parent=bob')"
chkc "不能挂到下级下" "不能挂到自己的下级用户下" "$(sf $J /admin/users/parent 'username=alice&parent=bob')"
chkc "根管理员不下移" "根管理员必须保持在顶层" "$(sf $J /admin/users/parent 'username=admin&parent=alice')"
rm -f /tmp/ut_dave.txt
curl -s --noproxy '*' -o /dev/null -c /tmp/ut_dave.txt -X POST $A/login -d "username=dave&password=dave12345&next=/admin/users"
chkc "不能改自己的上级" "不能调整当前登录用户的上级" "$(sf /tmp/ut_dave.txt /admin/users/parent 'username=dave&parent=alice')"
chk  "bob 移到根(留空=根)" 303 "$(sc $J /admin/users/parent 'username=bob&parent=')"
users_page $J; chk "bob 上级变为 admin" "admin" "$(parentof bob)"
chk  "bob 移回 alice" 303 "$(sc $J /admin/users/parent 'username=bob&parent=alice')"
users_page $J; chk "bob 上级回到 alice" "alice" "$(parentof bob)"

echo "== 6. 删除保护 =="
chkc "删自己被拒"     "不能删除当前登录用户" "$(sf $J /admin/users/delete 'username=admin')"
chkc "删不存在的用户" "用户不存在"           "$(sf $J /admin/users/delete 'username=ghost')"
chkc "有下级不可删"   "该用户还有下级用户"   "$(sf $J /admin/users/delete 'username=alice')"
chkr $J /admin/users/delete 'username=carol' "删 carol 成功" 303
chkr $J /admin/users/delete 'username=erin'  "删 erin 成功"  303
chkr $J /admin/users/delete 'username=bob'   "删 bob 成功"   303
users_page $J
chkc "carol 已消失" "0" "$(grep -c '<strong>carol</strong>' $UP)"
chkc "erin 已消失"  "0" "$(grep -c '<strong>erin</strong>' $UP)"
chkc "bob 已消失"   "0" "$(grep -c '<strong>bob</strong>' $UP)"
chkc "alice 仍有下级(dave)" "该用户还有下级用户" "$(sf $J /admin/users/delete 'username=alice')"
chkr $J /admin/users/delete 'username=dave' "删 dave 成功" 303
chkr $J /admin/users/delete 'username=fred' "删 fred 成功" 303
users_page $J
chk "alice 已无下级" "0" "$(awk -v u=alice 'BEGIN{RS="<tr>"} $0 ~ ("<strong>" u "</strong>")' $UP | grep -c '下级')"

echo "== 7. 设备归属与删除保护 =="
SIM_ADDR=127.0.0.1:1690 SIM_MDVR=MDVR-T1 SIM_SN=UPS-BOX-1 SIM_INTERVAL=2 "$ROOT/target/release/simulator.exe" > /tmp/ut_sim.log 2>&1 &
SIMPID=$!
for i in $(seq 1 40); do
  sleep 0.5
  curl -s --noproxy '*' -b $J $A/api/tree | grep -q UPS-BOX-1 && break
done
chkc "模拟器设备已上线" "UPS-BOX-1" "$(curl -s --noproxy '*' -b $J $A/api/tree | grep -o 'UPS-BOX-1' | head -1)"
chkc "分配给不存在的设备" "设备不存在"     "$(sf $J /devices/UPS-NOPE/assign 'owner=alice&group=g')"
chkc "分配给不存在的用户" "归属用户不存在" "$(sf $J /devices/UPS-NOPE/assign 'owner=ghost&group=g')"
chkc "分配给 alice 成功" "已更新归属" "$(sf $J /devices/UPS-BOX-1/assign 'owner=alice&group=')"
chkc "有设备不可删" "该用户名下仍有设备" "$(sf $J /admin/users/delete 'username=alice')"
chkc "设备改回 admin" "已更新归属" "$(sf $J /devices/UPS-BOX-1/assign 'owner=admin&group=')"
chkr $J /admin/users/delete 'username=alice' "alice 可删除" 303
users_page $J; chkc "alice 已删除" "0" "$(grep -c '<strong>alice</strong>' $UP)"
kill $SIMPID 2>/dev/null

echo "== 8. 非管理员越权（RBAC 端到端）=="
chkr $J /admin/users 'username=gina&password=gina12345&role=viewer&parent=' "创建 gina(viewer)" 303
rm -f /tmp/ut_e.txt
curl -s --noproxy '*' -o /dev/null -c /tmp/ut_e.txt -X POST $A/login -d "username=gina&password=gina12345&next=/admin/users"
chk "gina 会话有效" 200 "$(getc /tmp/ut_e.txt /api/tree)"
for p in "/admin/users" "/admin/settings"; do
  chk "viewer 禁入 GET $p" 403 "$(getc /tmp/ut_e.txt $p)"
done
chk "viewer 禁入 POST /admin/users"          403 "$(sc /tmp/ut_e.txt /admin/users 'username=evil1&password=evil123&role=admin&parent=')"
chk "viewer 禁入 POST /admin/users/password"  403 "$(sc /tmp/ut_e.txt /admin/users/password 'username=gina&password=pwned123')"
chk "viewer 禁入 POST /admin/users/delete"    403 "$(sc /tmp/ut_e.txt /admin/users/delete 'username=gina')"
chk "viewer 禁入 POST /admin/users/parent"    403 "$(sc /tmp/ut_e.txt /admin/users/parent 'username=gina&parent=alice')"
chk "viewer 禁入 POST /devices/UPS-BOX-1/assign" 403 "$(sc /tmp/ut_e.txt /devices/UPS-BOX-1/assign 'owner=gina&group=')"
chk "viewer 可见 /api/tree" 200 "$(getc /tmp/ut_e.txt /api/tree)"
users_page $J
chkc "越权建号未生效" "0" "$(grep -c '<strong>evil1</strong>' $UP)"
chk "gina 口令未被改(303=仍可登录)" 303 "$(curl -s --noproxy '*' -o /dev/null -w '%{http_code}' -c /tmp/ut_g2.txt -X POST $A/login -d 'username=gina&password=gina12345&next=/x')"
chk "gina 账号未被越权删除" 200 "$(getc /tmp/ut_e.txt /api/tree)"

echo "== 9. 管理员自助改口令（自己的行也必须能改）=="
chkc "自己的行渲染重置表单" 'action="/admin/users/password"' \
     "$(users_page $J; awk -v u=admin 'BEGIN{RS="<tr>"} $0 ~ ("<strong>" u "</strong>")' $UP | grep -o 'action="/admin/users/password"' | head -1)"
chkc "admin 自助改口令成功" "密码已重置" "$(sf $J /admin/users/password 'username=admin&password=admin456')"
rm -f /tmp/ut_a2.txt
curl -s --noproxy '*' -o /dev/null -c /tmp/ut_a2.txt -X POST $A/login -d "username=admin&password=admin456&next=/monitor"
chk "新口令可登录" 200 "$(getc /tmp/ut_a2.txt /api/tree)"
rm -f /tmp/ut_a3.txt
curl -s --noproxy '*' -o /dev/null -c /tmp/ut_a3.txt -X POST $A/login -d "username=admin&password=admin123&next=/monitor"
chk "旧口令已失效(401)" 401 "$(getc /tmp/ut_a3.txt /api/tree)"
chk "新口令仍是 admin 角色" "admin" "$(roleof admin)"


echo
echo "结果: PASS=$pass FAIL=$fail"
exit $fail
