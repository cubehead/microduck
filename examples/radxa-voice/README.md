# Radxa ZERO 3W ARM64 离线中文识别

在板上有网络时准备一次。适用于 Debian/Ubuntu ARM64（`uname -m` 应为
`aarch64`）。已有 Python 3、venv、curl、unzip、arecord 时不用安装系统包。
Vosk 0.3.45 的 PyPI ARM64 wheel 要求 glibc 2.17+；中文小模型下载约 42 MB。
没在实际 ZERO 3W 上实测时延、内存或识别率。

```bash
sudo apt install alsa-utils python3-venv curl unzip  # 仅在缺工具时
python3 -m venv .venv
.venv/bin/pip install 'vosk==0.3.45'
curl -fL --retry 2 -O https://alphacephei.com/vosk/models/vosk-model-small-cn-0.22.zip
unzip -n vosk-model-small-cn-0.22.zip
```

`arecord -l` 看硬件卡；`arecord -L` 看 ALSA PCM 名称。先试板上的默认
输入；不对就用实际名称替换 `default`（例如 `plughw:CARD=你的声卡,DEV=0`）。
`plughw` 允许 ALSA 转成 16 kHz、单声道和 PCM16。

```bash
set -o pipefail
arecord -q -D default -t raw -f S16_LE -c 1 -r 16000 -d 12 |
  .venv/bin/python recognize.py --model vosk-model-small-cn-0.22
```

说“站立”“坐下”或“叫一声”，句间稍停。程序只在终端输出最终转录和是否恰好
匹配短语；不上传、不写录音文件、不启用常驻服务，也不连舵机或控制接口。删掉
`-d 12` 才会持续采集，Ctrl+C 停止。若 ALSA 报 `Device or resource busy`，
暂停占用该采集设备的进程后重试，不修改设备树、混音器或控制器配置。

- [Vosk 安装](https://alphacephei.com/vosk/install)
- [Vosk 官方模型](https://alphacephei.com/vosk/models)
- [Vosk 0.3.45 ARM64 wheel](https://pypi.org/project/vosk/0.3.45/#files)
