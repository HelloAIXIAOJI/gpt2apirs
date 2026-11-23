// 解析账号文件 -> 结构化 JSON（邮箱 / 密码 / TOTP 种子）
import { readFileSync, writeFileSync } from "node:fs";

const src = process.argv[2] ?? "C:\\Users\\aixiaoji\\Downloads\\FF261118684081.txt";
const out = process.argv[3] ?? "G:\\res\\login-lab\\accounts.json";

const lines = readFileSync(src, "utf8").split(/\r?\n/).map((l) => l.trim()).filter(Boolean);
const accounts = [];
const problems = [];
for (const [i, line] of lines.entries()) {
  const parts = line.split("----");
  if (parts.length !== 4) { problems.push({ line: i + 1, reason: "段数=" + parts.length }); continue; }
  const [email, password, totp, at] = parts.map((s) => s.trim());
  if (!/^[^@\s]+@[^@\s]+$/.test(email)) { problems.push({ line: i + 1, reason: "邮箱格式可疑：" + email.slice(0, 20) }); continue; }
  if (password.length < 6) { problems.push({ line: i + 1, reason: "密码过短" }); continue; }
  if (!/^[A-Z2-7]{16,64}$/.test(totp)) { problems.push({ line: i + 1, reason: "TOTP 不是 base32：" + totp.slice(0, 8) }); continue; }
  accounts.push({ email, password, totp, has_at: at.length > 200 });
}

// 去重（同邮箱只留一条）
const seen = new Set();
const unique = accounts.filter((a) => (seen.has(a.email) ? false : (seen.add(a.email), true)));

writeFileSync(out, JSON.stringify(unique.map(({ email, password, totp }) => ({ email, password, totp })), null, 2), "utf8");
console.log("读到", lines.length, "行 → 结构化", accounts.length, "条 → 去重后", unique.length, "条");
console.log("问题行:", problems.length, problems.slice(0, 5));
console.log("已写入", out);
console.log("带 AT 的账号数:", accounts.filter((a) => a.has_at).length);
