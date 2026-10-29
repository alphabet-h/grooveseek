# 31. 1 つの索引に書く process は OS の lock で 1 つに限る

- Status: accepted
- Date: 2026-10-29
- Deciders: プロジェクトオーナー
- Applies to: v1.22.0 では `groove index` と読む command (PR #408)。
  `groove serve` が加わるのは後の版

## 背景と課題

watcher は process の中の mutex を持ったまま disk を見て索引に書くので、1 つの `groove serve` の
中の 2 つの書き込みは割り込み合わない。別の **process** は割り込める: server が同じ
ナレッジベースを watch している間に走った `groove index` が row を読み、書き込むまでの間に
もう一方の process がその row を変え得る。[behavior.ja.md](../behavior.ja.md) はこれまでそう書き、
「書き手は同時に 1 つ」の規則を利用者に委ねていた。

窓が一番広いのは全件 rebuild の最後の prune である。walk が訪れなかった row をすべて消すので、
run の間にもう一方の process が書いた、実在する file の row が消え、それを戻す file の event も
来ない: 文書が黙って search から消える。直し方は次の害の順に合わせた:

1. 最悪: 実在する文書が、何も言わずに search から消える。
2. 次: 削除した文書の row が残り続ける。
3. 許容: 書き手が待つ、または理由を添えた Err で止まる。

## 判断の軸

- 「書き手は同時に 1 つ」を、利用者が守る規則から仕組みに変える。
- 書き込み点を一つずつ作り直さずに、process をまたぐ窓を閉じる。特に watcher が組み直した
  mutex の区間を崩さない。
- 黙って直すより、止めて知らせる。
- crash や kill の後に手作業を要らなくする。
- Windows / macOS / Linux で同じに動き、依存を増やさない。

## 検討した選択肢

1. **2 つの書き手を正しく並走させる: 書き込み点ごとに `BEGIN IMMEDIATE` の中で見直す。**
   文書を失わない点と網羅の点では最も高く、費用の点では最も低かった: 変更はおよそ 5 回分、
   file の rename の 3 段への組み直し、prune のための walk の「この path を集めるか」の規則の
   写しが要り、写しと walk がずれると prune されない row が永久に残る。watcher は search も取る
   mutex を持ったまま、別 process の SQLite の lock を (busy timeout の 30 s まで) 待ち、
   すべての transaction の意味が一度に変わる。却下。
2. **楽観的な並行制御: trigger が保つ row の世代 (`row_gen`) と compare-and-swap の書き込み。**
   1 つの場合 (walk が見落とした後に同じ内容で作り直された file) に黙って失うことを自ら認め、
   幽霊の反例があり、metadata だけの更新を見ず、30-45 か所 (案自身の推定) で look の前に token を
   読む順序に頼る (1 か所でも逆だと黙って素通りする)。schema と trigger は覆すのが高い。却下。
   将来の防御層の候補として、点数と弱点ごとここに残す。
3. **`index_meta` に lease の row を置き、heartbeat で生かす。** crash した holder は row を残す
   ので、引き継ぐには両方向に誤り得る timeout が要り、lease を取ること自体が DB への書き込みに
   なる。誰が lease を持つかを決める手段としては却下。heartbeat は表示用の file の鮮度を示す
   ためだけに残す (下記)。
4. **`fs4` crate。** std は Rust 1.89 から `File::try_lock` と `File::unlock` を持つ。依存を
   増やさないので却下。
5. **DB の隣の file に OS の排他 lock を取る。** 採用。crash や kill を含め、OS が process と
   一緒に外す。

5 の変種で採らなかったもの:

- **lock を取れなかった server が、holder が去った後に引き継ぐ** (全件の追いつきつき)。設定が
  書き手と同じかの照合、全件の追いつき、watch の登録待ちが要る。後の判断に回した。
- **読むだけの server の `rebuild_index` が、holder が去った後に lock を取る。** その設定
  (parser、ナレッジベース、除外、サイズ上限) は索引を作った書き手と違い得て、prune は自分の
  walk が集めない row をすべて消すので、実在する file の row も消える。最もありそうな呼び手は、
  server の知らせを読んだ agent で、利用者の判断ではない。
- **server が model の DL の後に lock を取る。** 初回の DL の間に hook や timer から走った
  `groove index` が、誰も気づかないまま server を restart まで reader にする。DL の前なら、
  その run が exit 3 で止まって理由を出す。
- **holder が去ったかを、lock を取って放して確かめる。** その瞬間に起動した server は restart
  まで reader になり、その瞬間の `groove index` は exit 3 になる。

3 つの設計は、審査員 3 人が 5 軸 (各 10 点、計 150 点) で採点した:

| 案 | 審査 1 (喪失/幽霊/網羅/費用/test) | 審査 2 | 審査 3 | 合計 | 平均 |
|---|---|---|---|---|---|
| **single-writer-enforced (採用)** | 8/8/8/6/8 = 38 | 7/8/7/7/8 = 37 | 8/6/7/8/8 = 37 | **112** | 37.3 |
| per-point-immediate (look-under-IMMEDIATE) | 8/7/8/3/7 = 33 | 8/7/8/3/6 = 32 | 9/8/9/2/6 = 34 | 99 | 33.0 |
| optimistic-cas-epoch (OCC、`row_gen`) | 5/6/6/4/7 = 28 | 6/6/8/5/7 = 32 | 7/7/8/4/7 = 33 | 93 | 31.0 |

| 軸 (平均) | lease | look-under-IMMEDIATE | OCC |
|---|---|---|---|
| 黙って失わない | 7.67 | **8.33** | 6.00 |
| 幽霊を残さない | 7.33 | 7.33 | 6.33 |
| 網羅 | 7.33 | **8.33** | 7.33 |
| 費用と危険 | **7.00** | 2.67 | 4.33 |
| test のしやすさ | **8.00** | 6.33 | 7.00 |

## 決定

**v1.22.0 で入るもの。** lock を取るのは `groove index` だけで、DB を `query_only` で開くのは
読む command (`status`・`search`・`graph`・`doctor`・`eval`・`tune`) だけである。`groove serve`
はまだ加わらない: これまでどおり lock を取らずに DB を書ける形で開くので、同じ索引に対して
`groove index` が並んで走れ、両方が書ける。下の箇条と結果のうち server について書いた所は、
`groove serve` が lock を取る版の設計であって、v1.22.0 の挙動ではない。

- **lock。** DB の隣の `.groove.db.writer-lock` (DB の path に `.writer-lock` を足したもの。
  `.groove.db` が symlink なら、この file と下の 2 つはその先の file の名前で、その隣に置く。
  SQLite は symlink の先を開くので、別名ごとに lock が分かれると 2 つの書き手が同じ DB に並ぶため。
  path 全体 (file の symlink と、その上の directory の symlink / junction) を lease を取るときに
  1 度だけ解決し、lock・holder の json・holder の open はすべてその解決済みの path を使うので、後で link を付け替えても
  書き手は自分の lock から外れない。hard link で 1 つの DB に 2 つの名前を付ける構成は扱わない:
  名前ごとに journal と WAL が別になるので、2 つの名前で開くこと自体を SQLite が未定義の動作としている
  ([How To Corrupt An SQLite Database File の 2.6 節](https://www.sqlite.org/howtocorrupt.html)))。
  0 byte で、std の `File::try_lock` (Unix は `flock`、Windows は `LockFileEx`) で排他に lock する。
  **消さない**: 消すと、古い file を待っていた process と新しい file を作った process がそれぞれ
  lock を持ち、書き手が 2 つになる。
- **holder の情報は 2 つ目の file** `.groove.db.writer.json` に置く (pid、role、開始時刻、版、
  絶対 path にしたナレッジベース)。`.groove.db.writer.json.tmp-<pid>` に書いて rename で置き換え、
  放す時に best-effort で消す。file を分けるのは、Windows の `LockFileEx` が lock 中の範囲を他の
  handle から読めなくする (std は file 全体を lock する。推定) ので、lock file に書くと断られた側が
  読めないため。json は**表示専用**で、書くか・待つか・rebuild するかをこれで決めることはなく、
  古いことがある。
- **lock の呼び出し自体が失敗したら** (「未対応」を含む) 書き手は止まる。lock 無しでは何も
  書かない。
- **`groove index`** は DB・その WAL・embedding model に触れる前に lock を取る。他の process が
  持っていれば、持ち主と次の手を stderr に出して exit code `3` で止まる。`--wait <SECONDS>` は
  その秒数まで 250 ms ごとに取り直す (blocking の lock では上限を守れない)。助言は持ち主で変わる:
  同じナレッジベースを watch する server (その `rebuild_index` を呼ぶ)、1 回きりの run (終わるのを
  待つ)、DB の file を共有する別のナレッジベースの書き手 (その `rebuild_index` は使わない)、
  不明。run の本体は lock を受け取る 1 つの関数に置き、書ける接続 (counter の書き出しと checkpoint
  を含む) が lock より先に drop されるようにする: 順序を関数の境界で固定する。呼び出し側に後から
  `drop(lease)` を足すと黙って崩れる形にはしない。
- **読む側。** `status`・`search`・`graph`・`doctor`・`eval`・`tune` (と、`groove serve` が
  加わった後は書き手でない server) は、DB を `PRAGMA query_only` で開き、意図しない書き込みを、書かれる代わりに Err にする。読む側に
  要る 1 回きりの書き込み (schema の upgrade、embedding model の記録、context mode) は role
  `migration` の短い lock の下で行い、最大 2 s 待つ。lock が取られていれば、諦める前に 1 回だけ
  読み直す (その間に holder が同じ書き込みを済ませ得る)。`query_only` に断られた書き込みを他の
  Err と見分けるのは 1 か所だけ: 拡張コードがちょうど `SQLITE_READONLY` (8) で、接続自体は
  read-only で開いていないこと。
- **書ける接続は、持っている lock を証明として受け取る** (`open_writer`、
  `open_or_replace_corrupt_with`)。source の走査で、binary がそれ以外の場所で書き手を開かない
  ことを確かめられる。library の `Database::open` は意味を変えない: 書けて、lock を取らず、
  書き手を 1 つにするのは呼び出し側の責務。
- **watcher つきの `groove serve`** (後の版から。この節の冒頭を参照) は起動時に、model の DL の前に lock を取り、終了まで持つ。
  取れなかった server は restart まで reader のまま: watcher を持たず、MCP の tool
  `rebuild_index` を断り、そのことを `search` と `rebuild_index` の応答ごと (`writer_notice`) と
  `/api/admin/status` で知らせる。`--no-watch` の server は `rebuild_index` の呼び出しごとにだけ
  lock を取る。終了時に watcher は abort しない: batch の境界で止めて await し、接続を
  `query_only` に戻してから lock を放す。

## 結果と代償

server が lock を持つ・待つことに関わる結果は、`groove serve` が加わる版から始まる。v1.22.0 の
server はどちらもしない。

- **`groove index` に exit code `3` ができた**: 他の groove process が索引に書いている、または
  `--wait` の期限が切れた。`groove serve` が lock を取るようになると、watch する server と並んで
  走っていた hook や timer の `groove index` は `3` で止まる。v1.22.0 ではまだ server と並んで走る。
- **他の process が lock を持っている間に起動した server は、restart まで書かない。** その間に
  削除した file の row は、restart とその後の rebuild まで残る。
- **watch する server の初回の model の DL の間** (BGE-M3 は約 2.3 GB)、同じ索引への
  `groove index` は `3` で止まる。
- **参加しない書き手は止まらない**: 古い binary、library の `Database::open` を呼ぶ program、
  手で打つ `sqlite3`。
- **固まった holder は lock を持ち続ける**。外すにはその process を止めるしかない。OS は process
  の終了や kill で lock を外す。lock file と古い json は残ってよく、片付けは要らない。
- **助言は持ち主で変わる**: 持ち主が同じナレッジベースを watch する server のときだけ、索引を
  最新に保つと書き、その `rebuild_index` を勧める。1 回きりの run、別のナレッジベース、不明の
  ときは勧めない。今の持ち主は知らせのたびに json から読み直す。
- **watcher が止まった書き手の server も、自分の `rebuild_index` は断らない**: その設定は watcher
  が止まる前に索引を書いていたものと同じで、断ると restart の他の唯一の手を奪う。断るかは、
  引き継ぎと同じ後の判断に回した。
- **json は書き手の server について `heartbeat_at` と `watching` を持つ**。server は 5 s ごとに
  書き直し、読む側は 15 s より古い heartbeat を古いとみなす。出荷している server の止め方
  (tray の Stop と Restart は process を terminate する。systemd は SIGTERM) は後片付けを飛ばす
  ので json が残る。heartbeat が無いと、読む側は去った server が索引を最新に保っていると言い
  続ける。どちらの field も表示専用。
- **1 writer の保証はローカルディスク上の DB に限る。** network file system 上では、`flock` や
  `LockFileEx` が効くかは file system 次第になる。知られている例: Linux の NFS では `flock` が
  `fcntl` の lock で模され、同じ process の 2 つの handle が衝突しなくなる。
- **build には Rust 1.89 以上が要る** (`File::try_lock` のため)。`rust-version = "1.89"` を宣言した。
- file の名前 (`.groove.db.writer-lock`、`.groove.db.writer.json` とその一時 file) と exit code
  `3` は [stability.ja.md](../stability.ja.md) で凍結する。`groove service uninstall --purge` が
  消すのは `.groove.db` だけで、lock file は残る。
- **見直す時**: 去った書き手を server が引き継ぐ必要が出た時、network file system 上の DB を
  対象にする時、または参加しない書き手が珍しくないと分かった時。
- **test が守る場所**: `grooveseek/src/writer_lease.rs` (lock、json、助言)、
  `grooveseek/src/db/lease.rs` (書き手の open、`query_only` とその拒否、1 回きりの書き込み)、
  `grooveseek/tests/writer_lease_cli.rs` (`groove index`、`--wait`、exit 3、kill された holder)、
  `grooveseek/tests/writer_lease_sources.rs` (書き手は lock の下でだけ開く)。

## 参考

- 「書き手は同時に 1 つ」が規則だった [behavior.ja.md](../behavior.ja.md)。`groove index --wait`
  と exit code は [usage.ja.md](../usage.ja.md)。
- Rust std の [`File::try_lock`](https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock)。
- SQLite の [`PRAGMA query_only`](https://www.sqlite.org/pragma.html#pragma_query_only)。
- English version: [0031-one-writer-per-index-by-an-os-lock.md](0031-one-writer-per-index-by-an-os-lock.md)
