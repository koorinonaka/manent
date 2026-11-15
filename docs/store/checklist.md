# Microsoft Store 提出のチェックリスト

ユーザーが上から順に行う。1 行 1 操作。Partner Center の画面の名前は 2026-11 時点の英語表記で書いた。名前が変わっていたら、近い項目を探す。

## A. 開発者アカウントを作る

1. https://storedeveloper.microsoft.com を開き、個人 (Individual) の開発者アカウントの登録を始める。
2. 登録に使う Microsoft アカウントでサインインする。
3. 発行者の表示名 (Publisher display name) を入れる。ストアに作者名として出る。
4. 本人確認の手順を終える。登録料を求められたら、払うかどうかをその場で決める。
5. 登録の完了のメールを待ち、Partner Center (https://partner.microsoft.com/dashboard) にサインインできることを確かめる。

## B. 名前を予約する

1. Partner Center で「Apps and games」を開く。
2. 「New product」から「MSIX or PWA app」を選ぶ。
3. 名前に `Manent` を入れ、「Check availability」を押す。
4. 使えると出たら「Reserve product name」を押す。使えないと出たら、ここで止めて別の名前を決める。

## C. Identity の値を取ってリポジトリに書く

1. 予約した製品の「Product management」から「Product identity」を開く。
2. `Package/Identity/Name` の値を写す。
3. `Package/Identity/Publisher` の値 (`CN=` で始まる) を写す。
4. `Package/Properties/PublisherDisplayName` の値を写す。
5. 3 つの値を、root/t261110-4d52 が作る `packaging/` の設定ファイルの該当する項目に書く。
6. root/t261110-4d52 の手順でパッケージを作り直し、`.msixupload` を用意する。

## D. 提出の中身を入れる

1. 製品の「Start your submission」を押す。
2. 「Pricing and availability」で価格を「Free」にする。
3. 同じ画面で、公開する市場を選ぶ (既定はすべての市場)。
4. 「Properties」でカテゴリを選ぶ (案: Productivity、またはUtilities & tools)。
5. 同じ画面のプライバシーポリシーの URL に `https://koorinonaka.github.io/manent/privacy.html` を入れる。
6. 同じ画面の Web サイトに `https://koorinonaka.github.io/manent/` を、サポートの連絡先に `https://github.com/koorinonaka/manent/issues` を入れる。
7. 「Age ratings」で質問票 (IARC) に答える。Manent は利用者どうしのやり取り、購入、位置情報、暴力や性的な内容を持たない。
8. 「Packages」に `.msixupload` を上げ、対象の端末を Desktop だけにする。
9. パッケージが `runFullTrust` を宣言しているなら、「Submission options」の制限付きの機能の説明に「Win32 のデスクトップアプリで、画面のキャプチャとトレイへの常駐に要る」と書く。
10. パッケージが `unvirtualizedResources` を宣言しているなら、同じ説明に「GitHub 版と同じ設定ファイル `%APPDATA%\Manent\settings.toml` を共有するため、このフォルダ 1 つだけを書き込みの仮想化から外す。ほかのフォルダは仮想化のまま。」と書く。
11. 「Store listings」で日本語 (ja-jp) を追加し、`docs/store/listing.ja.md` の各節を同じ名前の欄に貼る。
12. 「Store listings」で英語 (en-us) を追加し、`docs/store/listing.en.md` の各節を同じ名前の欄に貼る。
13. 各言語にスクリーンショットを上げる。画像は `docs/store/screenshots/<言語>/` にあり、順番は掲載情報の原稿の「スクリーンショット」の節にある。

## E. 提出する

1. 提出の概要の画面で、すべての項目が「Complete」になっていることを確かめる。
2. 「Submit to the Store」を押す。
3. 審査の結果のメールを待つ。不合格なら、メールにある理由をリポジトリのタスクに起こす。
   - 不合格の理由が `unvirtualizedResources` なら、`packaging/AppxManifest.xml` から宣言を 2 つ (`virtualization:ExcludedDirectory` の塊と `unvirtualizedResources` の capability) と `virtualization` の名前空間を外す。
   - `scripts/New-Msix.ps1` で `.msixupload` を作り直し、D の 10 の説明を消して出し直す。
   - そのときは、`docs/design.md` の既知の制限を元の形 (ストア版が先に起動した PC では設定が分かれる) に戻すタスクを起こす。

## 提出の前に済ませること

- `site/privacy.html` が gh-pages に出ていて、上の URL で開けること。
- ストア版が更新の確認をしないこと (root/t261110-0a7e)。プライバシーポリシーは、ストア版は通信しないと書いている。
