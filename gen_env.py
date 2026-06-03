"""首次启动时生成高位随机、互不冲突的空闲端口,写入 .env 持久化(重启保持不变)。"""
import secrets, socket, random

def free_high_port(used):
    for _ in range(500):
        p = random.randint(20000, 60000)
        if p in used:
            continue
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        try:
            s.bind(("127.0.0.1", p))
            s.close()
            used.add(p)
            return p
        except OSError:
            s.close()
    raise SystemExit("找不到空闲高位端口")

u = set()
print("COMPOSE_PROJECT_NAME=vpnmgr")
print(f"UI_PORT={free_high_port(u)}")
print(f"MIHOMO_PORT={free_high_port(u)}")
print(f"MIHOMO_CTRL_PORT={free_high_port(u)}")
print(f"MIHOMO_SECRET={secrets.token_hex(16)}")
print("PORT_RANGE_LOW=20000")
print("PORT_RANGE_HIGH=60000")
