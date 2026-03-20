# 第二十章：常见问题

本章汇总 ZeroSpider 使用过程中的常见问题及解决方案。

---

## 目录

1. [安装问题](#安装问题)
2. [配置问题](#配置问题)
3. [使用问题](#使用问题)
4. [渠道问题](#渠道问题)
5. [性能问题](#性能问题)
6. [错误代码参考](#错误代码参考)

---

## 安装问题

### Q: 编译失败，提示找不到编译器

**错误信息**：
```
error: linker 'cc' not found
```

**解决方案**：

```bash
# Ubuntu/Debian
sudo apt install build-essential

# Fedora/RHEL
sudo dnf group install development-tools

# macOS
xcode-select --install
```

### Q: Rust 版本不兼容

**错误信息**：
```
error: Rust version too old
```

**解决方案**：

```bash
# 更新 Rust
rustup update stable

# 设置默认版本
rustup default stable
```

### Q: 找不到 zerospider 命令

**错误信息**：
```
zerospider: command not found
```

**解决方案**：

```bash
# 添加到 PATH
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.bashrc
source ~/.bashrc

# 或重新安装
cargo install --path .
```

---

## 配置问题

### Q: API 密钥无效

**错误信息**：
```
Error: Unauthorized - Invalid API key
```

**检查步骤**：

1. 确认密钥正确
   ```bash
   zerospider auth status
   ```

2. 检查环境变量
   ```bash
   echo $OPENAI_API_KEY
   ```

3. 重新配置
   ```bash
   zerospider auth paste-token --provider openai
   ```

### Q: 配置文件找不到

**错误信息**：
```
Error: Config file not found
```

**解决方案**：

```bash
# 重新初始化
zerospider onboard

# 或手动创建目录
mkdir -p ~/.zerospider
```

### Q: 配置格式错误

**错误信息**：
```
Error: Failed to parse config
```

**解决方案**：

```bash
# 检查 YAML 语法
zerospider config validate

# 查看配置示例
zerospider config schema
```

---

## 使用问题

### Q: AI 不回复消息

**可能原因**：
1. 服务未运行
2. 用户不在白名单
3. 模型调用失败

**检查步骤**：

```bash
# 1. 检查服务状态
zerospider status

# 2. 检查渠道
zerospider channel list

# 3. 检查认证
zerospider auth status

# 4. 查看日志
tail -f ~/.zerospider/logs/zerospider.log
```

### Q: AI 回答不正确

**可能原因**：
1. 问题不够清晰
2. 模型不适合该任务
3. 上下文丢失

**解决方案**：

```
# 提供更清晰的提示
用户: 我想实现一个功能：[具体描述]
     要求：
     1. [要求1]
     2. [要求2]

# 检查当前模型
/model

# 清除上下文重新开始
/clear
```

### Q: 记忆功能不工作

**症状**：AI 记不住之前的对话

**检查步骤**：

```bash
# 检查记忆配置
zerospider status | grep -A5 "记忆系统"

# 查看记忆存储
ls ~/.zerospider/memory/

# 检查数据库
sqlite3 ~/.zerospider/memory/memory.db "SELECT COUNT(*) FROM memories;"
```

---

## 渠道问题

### Q: Telegram Bot 不回复

**检查清单**：

1. Bot Token 是否正确
   ```bash
   zerospider channel doctor
   ```

2. 用户是否在白名单
   ```yaml
   channels:
     telegram:
       allowed_users:
         - "your_username"
   ```

3. 服务是否运行
   ```bash
   zerospider status
   ```

### Q: Discord Bot 上线但不回复

**可能原因**：缺少消息权限

**解决方案**：

1. 进入 Discord 开发者门户
2. 选择你的应用 → Bot
3. 启用 "Message Content Intent"
4. 重新邀请 Bot 到服务器

### Q: 微信无法接入

**说明**：个人微信无法直接接入，需要使用企业微信

**解决方案**：

```yaml
channels:
  wecom:  # 企业微信
    enabled: true
    corp_id: "your_corp_id"
    agent_id: 1000001
    secret: "your_secret"
```

---

## 性能问题

### Q: 响应速度慢

**可能原因**：
1. 网络延迟
2. 模型推理慢
3. 服务器性能不足

**优化方案**：

```yaml
# 1. 使用更快的模型
provider: openrouter
model: openrouter/auto-fast

# 2. 启用智能选择
routing:
  smart_selection: true
  preference: speed

# 3. 使用 Groq（极快）
provider: groq
model: llama-3.1-70b-versatile
```

### Q: 内存占用高

**检查内存使用**：

```bash
zerospider status --memory
```

**优化方案**：

```yaml
# 减少缓存
memory:
  cache_size: 100

# 限制上下文
agent:
  max_context_tokens: 4000
```

### Q: Token 消耗太快

**查看使用统计**：

```bash
zerospider stats
```

**优化方案**：

```yaml
# 使用更便宜的模型
provider: deepseek
model: deepseek-chat

# 启用智能选择（自动选择便宜的）
routing:
  smart_selection: true
  preference: cost
```

---

## 错误代码参考

| 错误代码 | 说明 | 解决方案 |
|----------|------|----------|
| `E001` | 配置文件不存在 | 运行 `zerospider onboard` |
| `E002` | API 密钥无效 | 检查并重新配置密钥 |
| `E003` | 网络连接失败 | 检查网络或代理设置 |
| `E004` | 模型不可用 | 检查模型名称是否正确 |
| `E005` | 权限不足 | 检查用户白名单配置 |
| `E006` | Token 超限 | 等待重置或升级套餐 |
| `E007` | 上下文过长 | 清除对话或减少消息 |
| `E008` | 工具执行失败 | 检查工具权限设置 |
| `E009` | 文件访问被拒绝 | 检查文件路径权限 |
| `E010` | 认证已过期 | 重新登录认证 |

---

## 获取帮助

### 查看日志

```bash
# 实时日志
tail -f ~/.zerospider/logs/zerospider.log

# 错误日志
cat ~/.zerospider/logs/error.log
```

### 运行诊断

```bash
# 全面诊断
zerospider doctor

# 检查模型
zerospider doctor models

# 检查渠道
zerospider channel doctor
```

### 社区支持

- **GitHub Issues**: https://github.com/ailib-official/zerospider/issues
- **文档**: https://github.com/ailib-official/zerospider#readme
- **社区论坛**: https://github.com/ailib-official/zerospider/discussions

---

## 更多帮助

- [命令参考](./18-commands.md)
- [配置参考](./19-config.md)
- [安全设置](./17-security.md)

---

[← 上一章：配置参考](./19-config.md) | [返回目录](./README.md)
