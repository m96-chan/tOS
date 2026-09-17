# Android 内で完結する Debian VM

調査・実装日: 2026-09-17。対象は Pixel 10a、Android 17 / API 37、
`CP2A.260805.005`、製品版 `user` ビルド、root なし・ブートローダー未解除。

## 現在の構成

**初回に ADB で開発用権限を付与すると、tOS のアプリ UID から AVF の
非保護 Debian VM を直接起動できる。標準ターミナルの起動は不要。**

`android/build-apk.sh` はこの VM 版を生成する。Debian の永続ディスク、
Linux カーネル、初回セットアップ用パッケージを APK に同梱し、
foreground service が起動・通信・停止を管理する。

実機では初回セットアップ、主要ツール、HTTPS、apt update/install、
複数ペインとサイズ変更、正常終了後のデータ保持を確認した。
公式イメージの ARM64 起動オプションに合わせた版で、8 回の再起動を含む
32 チェックが成功。画面でも画像・日本語・入力・上下分割を確認した。

```text
tOS APK / Android の画面・IME / Rust compositor
  └─ VmService（アプリ UID）
       └─ /apex/com.android.virt/bin/vm run
            └─ AVF / crosvm / ハードウェア仮想化
                 └─ Debian / systemd / tOS guest agent / 各ペインの PTY
```

APK のインストールだけで動作する構成ではない。初回設定:

```sh
adb -s DEVICE_SERIAL shell pm grant io.github.m96chan.tos android.permission.MANAGE_VIRTUAL_MACHINE
adb -s DEVICE_SERIAL shell pm grant io.github.m96chan.tos android.permission.USE_CUSTOM_VIRTUAL_MACHINE
```

この Pixel では両権限の付与に成功した。root や端末全体の非公開 API 制限・
SELinux 設定の変更は行っていない。端末・OS をまたいで同じ権限付与と
動作を保証する結果ではない。

## Nyandroid との違い

[Nyandroid の VmConnector](https://github.com/m96-chan/Nyandroid/blob/main/app/src/main/java/dev/nyandroid/terminal/backend/VmConnector.kt)
は `android.virtualization.VM_TERMINAL` Intent で標準ターミナルを起動し、
その VM を SSH で探す。
[AvfVmBackend](https://github.com/m96-chan/Nyandroid/blob/main/app/src/main/java/dev/nyandroid/terminal/backend/AvfVmBackend.kt)
の直接起動部分は調査時点で `TODO` だった。

tOS は専用ディスクを app-private storage に持ち、標準ターミナルの
Activity・サービス・ディスク・SSH 設定を利用しない。AVF という OS の
仮想化サービスへの依存はあるが、別のターミナルアプリへの依存はない。

## 起動経路の調査結果

| 項目 | Pixel での結果 |
| --- | --- |
| AVF feature | 対応 |
| 保護 / 非保護 VM | 両方対応、capabilities = 3 |
| 通常インストール直後の VM 権限 | 未付与 |
| 上記 2 権限の ADB grant | 成功 |
| Java `VirtualMachineManager` | 取得・能力照会可能 |
| カスタムイメージ Builder | 通常アプリから必要なコンストラクター・メソッドが見えない |
| AVF `vm` コマンド | アプリ UID から実行可能 |
| 独自 Linux initramfs | 起動・双方向入出力・正常電源断を確認 |

最初の独立検証 APK (`io.github.m96chan.tos.avfprobe`) は、OS 同梱の
Microdroid カーネルと NDK で静的リンクした `/init` を使用した。
`TOS_AVF_GUEST_BOOT_OK`、入力のエコー、`VM ended: Shutdown` と
ホスト終了コード 0 を確認し、検証後にアンインストールした。

この Microdroid カーネルにはネットワーク機器の機能が省かれていたため、
Debian 版は Google 配布の AVF 用 Linux 6.12 カーネルを同梱する。
カーネル・配布物の出典と固定ハッシュは [vm/README.md](vm/README.md) と
[vm/prepare-kernel.sh](vm/prepare-kernel.sh) に記録している。

## Debian の通信と表示

OS の `vm` CLI の raw config では、標準 TerminalApp と同じネットワーク
設定を公開していない。このため、ゲスト TAP と Android 側 libslirp を
専用フレームで接続する IPv4 NAT を実装した。アプリには INTERNET 権限
だけが必要で、Android の VPN やホスト TAP を作成しない。

画面は従来の Android 版 compositor を使用し、各ペインをゲストの PTY に
接続する。文字サイズ・IME・画面分割をそのまま使い、ツールの実行場所を
Debian に移した。起動画像は Kitty graphics の直接転送で渡す。
ゲストのファイルパスを Android のファイルパスとして読ませない。
他アプリの画像表示でも直接転送が必要で、ゲスト内のファイルパスを
指定する転送方式は未対応。

起動時の ready ハンドシェイク後に、入力・出力・リサイズ・ネットワーク
フレームを流す。hvc0 を通信専用、hvc2 をカーネル・systemd のログ用に
分離し、CLI 自体の標準出力も別ログへ送る。混在するとバイナリフレームを
壊すため、この分離は必須。

## 配布上の制約

`/apex/com.android.virt/bin/vm` は一般アプリ向けの安定した公開 SDK では
ない。OS 更新ごとに互換性を確認する必要がある。今回の対象は AVF 対応
Pixel であり、非対応端末や初回 ADB 設定もできない環境の代替エンジンは
実装していない。

ゲストの root は Android の root ではない。ゲストディスクは APK 更新で
維持するが、アプリのアンインストールで失われる。実行中プロセスの復元、
Android 文書共有、受信ポート転送、ディスク拡張の UI は未実装。

詳細なビルド・インストール・実機テスト手順は [README.md](README.md)。

参考:
[AVF API と開発用権限](https://android.googlesource.com/platform/packages/modules/Virtualization/+/HEAD/libs/framework-virtualization/README.md)、
[AVF 権限定義](https://android.googlesource.com/platform/packages/modules/Virtualization/+/refs/heads/main/android/android.system.virtualmachine.res/AndroidManifest.xml)、
[標準ターミナルの Debian 構成](https://source.android.com/docs/core/virtualization/usecases#linux-development-environment)、
[VM CLI の入出力](https://android.googlesource.com/platform/packages/modules/Virtualization/+/refs/heads/main/android/vm/src/run.rs)、
[仮想コンソールの接続](https://android.googlesource.com/platform/packages/modules/Virtualization/+/refs/heads/main/android/virtmgr/src/crosvm.rs)。
