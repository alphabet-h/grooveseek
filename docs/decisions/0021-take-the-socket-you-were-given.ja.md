# 21. 渡された socket だけを受け、自分では作らない

- Status: accepted
- Date: 2026-09-18
- Deciders: プロジェクトオーナー
- Applies to: v1.11.0

## 背景と課題

GrooveSeek は誰も認証しない。`--i-know` 無しで非 loopback な bind を求められたときに
出す拒否文自体がそう書いている (`grooveseek/src/transport/mod.rs:397-406`) —
**そのポートに到達できる者は、知識ベース全体を読める**。したがって到達性がアクセス制御の
すべてであり、v1.10.0 まで到達性とは TCP ポートのことだった。loopback のポートは
そのホスト上のすべてのアカウントに開いているので、1 台に複数の daemon を
(文書の束ごとに 1 つ、それぞれ 1 人の呼び出し元のために) 立てても、
**どのポートが誰のものかという取り決め以上に強いものでは分かれていなかった**。

ファイルシステム上の socket は、この問いをカーネルが既に答えている場所へ移す。
所有者と mode は `connect(2)` の時点で、最初の 1 バイトが届く前に、
他のあらゆるファイルを守っているのと同じコードによって検査される。
必要なのは、**daemon が起動する前に、正しい所有者と mode でそれを作る誰か**である。

この記録が答えるのは「`AF_UNIX` で serve するかどうか」ではない。
**誰がその socket を作るのか、そして約束された socket を受け取れなかったとき
`serve` は何をするのか**である。

## 判断の軸

- 誰が接続してよいかを決めるものは、**誰も認証しないプロセスの外**に置かねばならない
- socket の所有者と mode は作成時に固定される。したがって「誰が作るか」は
  実装の細部ではなく**アクセス制御の一部**である
- 同じコマンドラインが、親から継承したものによって 2 つの異なる場所で待ち受けてはならない
- **運用者が設定した場所とは別の場所で待ち受ける daemon は、起動を拒否する daemon より悪い** —
  運用者は自分が書いたアドレスを読んで、それを信じるからである
- `sd_listen_fds(3)` が受け渡しの手順と、それを安全にする検査を既に定めている。
  独自に発明すれば、間違えられる箇所が 2 つになる
- 本件の想定配置では unit ごとに `ListenStream=` は 1 つなので、
  「ちょうど 1 本」はそこでは何の代償にもならず、
  **すべての拒否が「読めた値」を名指しできる**状態を保てる
  (`grooveseek/src/transport/systemd_fd.rs:78-82`)

## 検討した選択肢

1. **`.socket` unit が既に bind した descriptor を、明示 opt-in で受け取る。
   受け取れなければ起動を拒否する。** — 採った案。

2. **socket activation を環境変数から自動検出する** (`LISTEN_FDS` が居れば使う)。
   却下。1 つのコマンドラインが 2 つの異なる意味を持つことになり、しかも
   **どちらになるかを決める変数は継承される**。activation された service から起動した shell、
   自分の descriptor を渡し込む supervisor、他者のために変数を設定した親 —
   どれも同じ形で `serve` に届く。`sd_listen_fds(3)` が最初に置いている検査は、
   まさに**環境変数が descriptor より遠くまで伝わる**から存在する
   (`grooveseek/src/transport/systemd_fd.rs:57-71`)。自動検出にすると、
   その検査を通り抜けることが「opt-in された経路」ではなく「通常の経路」になる。

3. **groove 自身が path に bind する** (`--unix-socket <path>`)。却下。
   動きはするが、**socket を作る主体が 2 つになる**。所有者と mode は、groove と
   unit ファイルと周囲の `umask` のうち先に到達したものが決めることになり、
   しかも配置ごとに違う主体が決める。本設計では socket を作るのは service manager の仕事であり、
   そこに置いたままにしておくことが**「誰がここへの到達可否を決めたか」に答えを 1 つだけ残す**。

4. **unit と groove の間に `systemd-socket-proxyd` を挟む** (systemd の man の
   namespace の例が採っている形)。proxy の 3 つの性質を根拠に却下。いずれも一次情報:

   - **接続ごとの timeout も `SO_KEEPALIVE` も無い**
     (<https://github.com/systemd/systemd/issues/23320>)。MCP の Streamable HTTP は
     接続を張り続けるので、半死の接続が `--connections-max=` の枠を食う
   - **資格情報を通さない**。`systemd-socket-proxyd(8)` の原文が
     "will not forward `SCM_RIGHTS`, `SCM_CREDENTIALS`, `SCM_SECURITY`, `SO_PEERCRED`,
     `SO_PEERPIDFD`, `SO_PEERSEC`, `SO_PEERGROUPS` and similar" と書いている
   - **1 つの proxy は 1 つの socket にしか対応しない**
     (<https://github.com/systemd/systemd/issues/15599>)。配置を 1 つ増やすたびに unit が増える

   4 つ目の懸念には裏付ける文書が無いので、**推定**としてここに残す:
   proxy は上流への接続を持つ前に接続を受け付けるので、呼び出し元の死活確認は
   「接続できた」と答えられてから待たされ、失敗にはならないと考えられる。
   **これは実測していない。**する必要も無かった — 上の 3 つで足りている。

   いずれにせよ daemon ごとにプロセスが 1 つ前に挟まる。descriptor を直接
   受け取る形なら 0 である。

5. **`SO_PEERCRED` で接続元の uid を照合する**。却下 — そして**この道が開いているのは
   選択肢 4 のおかげ**である。proxy を通さないことが、資格情報が届く状態を残している。
   axum の `Connected` trait は sealed ではなく
   (`axum-0.8.9/src/extract/connect_info.rs:80-83`)、axum 自身が `UnixListener` 向けの
   実装をコンパイル試験として持っている (`axum-0.8.9/src/serve/mod.rs:503-513`)。
   `IncomingStream::io()` (`axum-0.8.9/src/serve/mod.rs:436-439`) から `UnixStream` が取れ、
   その `peer_cred()` が答えを返す。

   **それでも採らない。** socket の mode が、同じ問いを**より早い段階でカーネルに**
   立てさせている。アプリケーション層でもう一度問えば、1 つの条件が
   **socket の mode とアプリケーションのコードという、食い違いうる別々の場所へ
   分かれる**。加えて `Connected::connect_info` は
   `io::Result` ではなく `Self` を返す (`axum-0.8.9/src/extract/connect_info.rs:82`) ので、
   `peer_cred()` が失敗したときにそれを申告する先が無い。既定値は
   **fail-open か、理由の言えない拒否**のどちらかにしかならない。

   **覆る条件**: 1 つの socket を複数の principal に開ける必要が出たとき
   (想定する呼び出し元とは別に監視エージェントを足す、外部の read-only 利用者を作る等)。
   そのとき mode はグループへ広げられ、**uid の区別をここで持つ必要が出る**。

## 決定

**`groove serve` は渡された socket で accept し、自分では socket を作らない。**

- **明示 opt-in**。`--systemd-socket`、または `[transport.http].systemd_socket = true`。
  どちらも無ければ `LISTEN_FDS` は誰も読まず、要求しない daemon は
  v1.11.0 より前のすべてのリリースと同じように振る舞う
- **TCP に落ちない**。socket を受け取れなければ `serve` は終了する。descriptor の検査は
  `sd_listen_fds(3)` が定める順序で行う: まず `LISTEN_PID` を自プロセスの pid と照合する
  (親から継承した変数は、そうしなければ**他人の descriptor**を掴ませる)、次に
  `LISTEN_FDS` がちょうど 1 であること、そして `SO_ACCEPTCONN` より先に `SO_TYPE` を見る
  (`ListenDatagram=` の unit を「listening でない」ではなく**型の誤り**として報告するため)
  (`grooveseek/src/transport/systemd_fd.rs:52-130`)
- **自前のアドレスとは排他**。`--bind` / `--port` / `[transport.http].bind` のいずれも、
  これと並んで立つことを拒否する (`grooveseek/src/transport/mod.rs:330-360`)。
  待ち受けアドレスが 2 つあるのは設定ではないし、黙って一方を勝たせれば
  **運用者は何も応答しないアドレスを読み続ける**ことになる
- **動く場所は protocol がある場所**。`LISTEN_FDS` を渡す service manager を持つ Unix —
  Linux の systemd、および同じ protocol を話す他のもの。**Windows ビルドはフラグもキーも拒否する**
  (`grooveseek/src/transport/mod.rs:307-309`)
- **family は descriptor から読む**。宣言させない。unit が bind した TCP socket は
  groove 自身が bind したものとまったく同じように扱われ (peer 検査も既定値の導出も含めて)、
  Unix socket は**アドレスを一切持たない listener** になる
  (`grooveseek/src/transport/systemd_fd.rs:213-223`)
- **socket ファイルは groove が管理するものではない**。`shutdown(2)` を呼ばず、
  path を unlink もしない。service manager が自分の descriptor の複製を持ち続けており、
  `systemd.socket(5)` は service が "must not unlink the socket from a file system" と書いている
- **依存を増やさない**。`getsockopt` / `getsockname` / `fcntl` を、この crate が
  `cfg(unix)` で既に持っている `libc` から呼ぶ。`libsystemd` や `listenfd` は足さない

## 結果と代償

- **listener がアドレスを持たない状態がありうるようになった**
  (`grooveseek/src/transport/http.rs:898-956`)。それを読む判断は、peer 規則・
  `Host` の既定・`Origin` の既定・起動時の行であり、いずれも
  `open_listener` が返す同じ `Option<SocketAddr>` から来る
  (`grooveseek/src/transport/http.rs:1046-1055`、`:1236`)。だから
  **見落としやすい場合** (unit が *TCP* の descriptor を渡してきた場合) を、
  一方では一通りに、その隣では別の通りに扱う、ということが起きない
- **peer 検査はフラグではなく型で分かれる**。`UnixListener` は
  `ConnectInfo<SocketAddr>` を持たないので、これが置き換えた `bool` は
  「有効」と読めているのに、守っているはずの条件が黙って素通りしていた。
  `PeerRule::UnixLocal` が Unix 側で、その意味は
  **「socket の所有者と mode が既に決めた」**であって、
  **「誰だか分からない」ではない** (`grooveseek/src/transport/http.rs:1681-1714`)
- **ここで試験が守れる範囲は、決定より狭い**。Unix listener では `PeerRule` の 3 値が
  観測上すべて同じ答えを返す。検査が読む extension がそもそも付かないからである。
  Unix listener に対する挙動試験が守るのは `Host` / `Origin` の配線であり、
  `admin_peer_rule` が listener ごとに何を返すかは単体試験が守る。
  **`run_http` が admin 経路にその戻り値を渡していること自体を守るのは、
  レビューだけである**
- **`Origin` の既定は port 無しの loopback の綴りになる**。名指す port が無いからである。
  **port を持たない allow-list の entry は、そのホストの全 port に一致する**
  (`grooveseek/src/transport/http.rs:673-681`) ので、`localhost` / `127.0.0.1` / `[::1]` を
  名乗る `Origin` は**どの port を載せていても通り**、それ以外の `Origin` は拒否される。
  リストを空にしないのは意図的で、空は「`Origin` を検証しない」の綴りだからである
- **`unsafe` が transport 層に入る**。ただし `systemd_fd.rs` の中だけで、そこの
  `unsafe` ブロックは、生の descriptor 番号を所有へ変える 1 行を除いてすべて
  `libc` の呼び出しである
  (`grep -n "unsafe {" grooveseek/src/transport/systemd_fd.rs` が
  `:93` / `:136` / `:139` / `:157` / `:273` を返す。`:336` 以降は `#[cfg(test)]`)。
  **所有権が生まれるのは `take_listener` の `:273` だけ**で、しかも
  所有せずにできる検査を先に済ませたあとである
  (`grooveseek/src/transport/systemd_fd.rs:253-274`)。`adopt`
  (`grooveseek/src/transport/systemd_fd.rs:213-223`) は `OwnedFd` を受け取るので、
  その中に `unsafe` は現れない
- **unit ファイルより先に運用者が越える version の下限ができる**。v1.10.0 以前は
  未知のキーを拒否するので、`systemd_socket` を書いた `groove.toml` は
  **それらのリリースをそもそも起動させない**。groove を先に上げ、それから unit を変える
- **到達性が groove の知らないものになる**。socket の path・所有者・mode、および
  unit がその周りに設定するものがアクセス制御であり、そのどれもプロセスの中からは見えない。
  groove はそれを検査せず、報告せず、**間違っていても警告できない** —
  非 loopback bind の警告は、この listener では見るものを持たない。
  これは運用者側に 1 手を課すことであり、この記録が黙っていてよい話ではない:
  `systemd.socket(5)` の `SocketMode=` の既定は `0666` なので、
  `ListenStream=` しか書かれていない unit が返すのは、
  **上の背景で挙げた loopback ポートとまったく同じ到達性**である
- **拒否文がインタフェースである**。fallback が無い以上、serve できない形はすべて
  起動失敗であり、そこで表示される文が運用者の手がかりのすべてになる。
  拒否文は ASCII を保ち、**期待した値だけでなく読み取れた値も名指す**

## 参考

- `sd_listen_fds(3)` — 受け渡しの protocol と検査の順序。`systemd.socket(5)` —
  `Accept=no`、socket を unlink しないこと、および `SocketMode=` の既定 `0666`。
  `systemd-socket-proxyd(8)` — proxy が転送しないもの
- [ADR-0009](0009-one-dns-rebinding-gate.ja.md) — ここでの既定値が流れ込む gate、および
  `Host` / `Origin` を GrooveSeek 自身が答える理由
- [deployment-topologies.ja.md](../deployment-topologies.ja.md) —
  listener の種類ごとに各経路が何を問うか
- `grooveseek/src/transport/systemd_fd.rs` (受け渡し)、
  `grooveseek/src/transport/mod.rs` (`HttpListen` / `resolve_systemd_listen` /
  `systemd_socket_supported`)、`grooveseek/src/transport/http.rs`
  (`open_listener` / `PeerRule` / `admin_peer_rule` /
  `effective_allowed_hosts_unix` / `effective_allowed_origins_unix`)
- 英語版:
  [0021-take-the-socket-you-were-given.md](0021-take-the-socket-you-were-given.md)
