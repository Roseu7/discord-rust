# Rust Discord Bot

RustとSerenityを使ったDiscordボット開発の学習プロジェクト。

## 機能

- Wordleヘルパー（`/wht` コマンド）
  - インタラクティブな単語入力UI
  - 推測結果の視覚的な記録（カラー絵文字）
  - 情報理論ベースの最適単語提案
  - Supabaseからの単語データベース読み込み

## 技術スタック

- **Rust** - メイン言語
- **Serenity** - Discord API
- **Shuttle** - デプロイ
- **Supabase** - データベース
- **Tokio** - 非同期ランタイム

## 使い方

1. `/wht` コマンドでボットを起動
2. 「新しい単語を入力」ボタンをクリック
3. 推測した5文字の英単語を入力
4. 各文字の結果をクリックして色を変更
5. 確定ボタンで次の推奨単語を取得

## アルゴリズム（Wordleヘルパー）

単語提案は以下の要素を考慮：

- 文字の多様性
- 英語の文字頻度
- 母音/子音バランス
- 情報ゲイン（エントロピー計算）

## 今後の予定

- 他の機能を追加予定
