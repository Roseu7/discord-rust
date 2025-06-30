use anyhow::Context as _;
use serenity::{all::{
        CreateActionRow,
        CreateButton,
        CreateCommand,
        CreateEmbed,
        CreateInputText,
        CreateInteractionResponse,
        CreateInteractionResponseMessage,
        CreateModal,
        Interaction,
        ModalInteraction,
        ButtonStyle,
        GuildId,
        InputTextStyle,
        ComponentInteraction,
        Colour,
        EditInteractionResponse,
    },
    async_trait};
use serenity::model::gateway::Ready;
use serenity::prelude::*;
use shuttle_runtime::SecretStore;
use tracing::info;
use serde::{Deserialize, Serialize};
use serde_json;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WordleGuess {
    word: String,
    results: Vec<LetterResult>, // 0: gray, 1: yellow, 2: green
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum LetterResult {
    Gray = 0,
    Yellow = 1,
    Green = 2,
}

#[derive(Debug, Clone)]
struct GameState {
    guesses: Vec<WordleGuess>,
    current_word: Option<String>,
    pending_result: bool,
    current_results: Vec<LetterResult>,
    last_suggestion: String,
}

// Supabaseのレスポンス用構造体
#[derive(Debug, Clone, Deserialize)]
struct WordRecord {
    id: i32,
    word: String,
}

#[derive(Debug, Deserialize)]
struct EmojiRecord {
    emoji_name: String,
    emoji_id: i64,
    discord_format: String,
}

// 単語評価用の構造体
#[derive(Debug, Clone)]
struct WordScore {
    word: String,
    score: f64,
    info_gain: f64,
}

struct Bot {
    client: reqwest::Client,
    discord_guild_id: GuildId,
    supabase_url: String,
    supabase_key: String,
    game_states: Arc<tokio::sync::RwLock<HashMap<u64, GameState>>>,
    emoji_cache: Arc<tokio::sync::RwLock<HashMap<String, String>>>,
    word_cache: Arc<tokio::sync::RwLock<Vec<WordRecord>>>
}

impl Bot {
    // Supabaseから単語リストを取得してキャッシュ
    async fn load_word_cache(&self) -> anyhow::Result<()> {
        let mut all_words = Vec::new();
        let mut offset = 0;
        let limit = 1000; // 1回のリクエストで取得する件数

        loop {
            let url = format!(
                "{}/rest/v1/words?select=id,word&limit={}&offset={}",
                self.supabase_url, limit, offset
            );

            info!("Fetching words from: {} (offset: {})", url, offset);

            let response = self.client
                .get(&url)
                .header("apikey", &self.supabase_key)
                .header("Authorization", format!("Bearer {}", self.supabase_key))
                .send()
                .await
                .context("Failed to send request to Supabase")?;

            info!("Response status: {}", response.status());

            if !response.status().is_success() {
                let error_text = response.text().await.unwrap_or_default();
                info!("Error response body: {}", error_text);
                return Err(anyhow::anyhow!("Supabase request failed: {}", error_text));
            }

            let response_text = response.text().await
                .context("Failed to read response text")?;

            let words: Vec<WordRecord> = serde_json::from_str(&response_text)
                .context("Failed to parse JSON response")?;

            let fetched_count = words.len();
            info!("Fetched {} words in this batch", fetched_count);

            all_words.extend(words);

            // 取得した件数がlimitより少ない場合、全件取得完了
            if fetched_count < limit {
                break;
            }

            offset += limit;
        }

        let mut cache = self.word_cache.write().await;
        *cache = all_words;

        info!("Successfully loaded {} word records in total", cache.len());
        Ok(())
    }

    // Supabaseから絵文字情報を取得してキャッシュ
    async fn load_emoji_cache(&self) -> anyhow::Result<()> {
        let url = format!("{}/rest/v1/emojis?select=emoji_name,emoji_id,discord_format", self.supabase_url);

        let response = self.client
            .get(&url)
            .header("apikey", &self.supabase_key)
            .header("Authorization", format!("Bearer {}", self.supabase_key))
            .send()
            .await?;

        let emojis: Vec<EmojiRecord> = response.json().await?;

        let mut cache = self.emoji_cache.write().await;
        for emoji in emojis {
            cache.insert(emoji.emoji_name.clone(), emoji.discord_format);
        }

        info!("Loaded {} emoji records", cache.len());
        Ok(())
    }

    // 制約に基づいて可能な単語をフィルタリング
    fn filter_words_by_constraints(&self, words: &[WordRecord], game_state: &GameState) -> Vec<WordRecord> {
        words.iter()
            .filter(|word_record| {
                let word = word_record.word.to_uppercase();
                // 5文字の単語のみを対象とする
                word.len() == 5 &&
                word.chars().all(|c| c.is_ascii_alphabetic()) &&
                self.is_word_possible(&word, game_state)
            })
            .cloned()
            .collect()
    }

    // 単語が制約を満たすかチェック
    fn is_word_possible(&self, word: &str, game_state: &GameState) -> bool {
        for guess in &game_state.guesses {
            if !self.word_matches_result(word, &guess.word, &guess.results) {
                return false;
            }
        }
        true
    }

    // 単語が特定の推測結果と一致するかチェック
    fn word_matches_result(&self, candidate: &str, guess: &str, results: &[LetterResult]) -> bool {
        let candidate_chars: Vec<char> = candidate.chars().collect();
        let guess_chars: Vec<char> = guess.chars().collect();

        if candidate_chars.len() != guess_chars.len() || guess_chars.len() != results.len() {
            return false;
        }

        // 緑色の制約をチェック（正しい位置）
        for (i, result) in results.iter().enumerate() {
            match result {
                LetterResult::Green => {
                    if candidate_chars[i] != guess_chars[i] {
                        return false;
                    }
                }
                _ => {}
            }
        }

        // 各文字の最小必要数と最大許可数を計算
        let mut min_required: HashMap<char, usize> = HashMap::new();
        let mut max_allowed: HashMap<char, usize> = HashMap::new();
        let mut forbidden_positions: HashMap<char, HashSet<usize>> = HashMap::new();

        // 推測結果を分析
        for (i, result) in results.iter().enumerate() {
            let letter = guess_chars[i];
            match result {
                LetterResult::Green => {
                    *min_required.entry(letter).or_insert(0) += 1;
                }
                LetterResult::Yellow => {
                    *min_required.entry(letter).or_insert(0) += 1;
                    forbidden_positions.entry(letter).or_insert_with(HashSet::new).insert(i);
                }
                LetterResult::Gray => {
                    // この文字が他の場所で緑や黄色になっていない場合、単語に含まれない
                    let letter_used_elsewhere = results.iter().enumerate().any(|(j, r)| {
                        j != i && guess_chars[j] == letter && matches!(r, LetterResult::Green | LetterResult::Yellow)
                    });

                    if letter_used_elsewhere {
                        // 他の場所で使われている場合は、その分だけ許可
                        let used_count = results.iter().enumerate()
                            .filter(|(j, r)| *j != i && guess_chars[*j] == letter && matches!(r, LetterResult::Green | LetterResult::Yellow))
                            .count();
                        max_allowed.insert(letter, used_count);
                    } else {
                        // 完全に含まれない
                        max_allowed.insert(letter, 0);
                    }
                }
            }
        }

        // 候補単語の文字数をカウント
        let mut candidate_counts: HashMap<char, usize> = HashMap::new();
        for &ch in &candidate_chars {
            *candidate_counts.entry(ch).or_insert(0) += 1;
        }

        // 最小必要数をチェック
        for (letter, min_count) in &min_required {
            if candidate_counts.get(letter).unwrap_or(&0) < min_count {
                return false;
            }
        }

        // 最大許可数をチェック
        for (letter, max_count) in &max_allowed {
            if candidate_counts.get(letter).unwrap_or(&0) > max_count {
                return false;
            }
        }

        // 禁止位置をチェック
        for (letter, positions) in &forbidden_positions {
            for &pos in positions {
                if pos < candidate_chars.len() && candidate_chars[pos] == *letter {
                    return false;
                }
            }
        }

        true
    }

    // 高度な単語提案システム
    async fn get_optimal_words(&self, game_state: &GameState) -> anyhow::Result<Vec<String>> {
        {
            let words = self.word_cache.read().await;
            info!("Total words in cache: {}", words.len());

            if words.is_empty() {
                info!("Word cache is empty, attempting to reload");
                drop(words); // ロックを解放

                if let Err(e) = self.load_word_cache().await {
                    info!("Failed to reload word cache: {:?}", e);
                    return Ok(vec!["SLATE".to_string(), "CRANE".to_string(), "AUDIO".to_string(), "ARISE".to_string(), "OUTER".to_string()]);
                }
            }
        }

        // 再度ロックを取得してフィルタリング
        let words = self.word_cache.read().await;
        if words.is_empty() {
            info!("Word cache still empty after reload");
            return Ok(vec!["SLATE".to_string(), "CRANE".to_string(), "AUDIO".to_string(), "ARISE".to_string(), "OUTER".to_string()]);
        }

        let possible_words = self.filter_words_by_constraints(&words, game_state);
        info!("Possible words after filtering: {}", possible_words.len());

        // フィルタリング結果の詳細ログ
        if possible_words.is_empty() {
            info!("No possible words found. Game state constraints:");
            for (i, guess) in game_state.guesses.iter().enumerate() {
                info!("  Guess {}: {} -> {:?}", i + 1, guess.word, guess.results);
            }

            // 制約なしで5文字の単語があるかチェック
            let five_letter_words: Vec<_> = words.iter()
                .filter(|w| w.word.len() == 5 && w.word.chars().all(|c| c.is_ascii_alphabetic()))
                .take(10)
                .collect();
            info!("Sample 5-letter words in database: {:?}", 
                five_letter_words.iter().map(|w| &w.word).collect::<Vec<_>>());

            // フォールバック：一般的な開始単語
            return Ok(vec!["SLATE".to_string(), "CRANE".to_string(), "AUDIO".to_string(), "ARISE".to_string(), "OUTER".to_string()]);
        }

        // 以下は既存のコードと同じ...
        if possible_words.len() == 1 {
            return Ok(vec![possible_words[0].word.to_uppercase()]);
        }

        if possible_words.len() <= 10 {
            return Ok(possible_words.iter().map(|w| w.word.to_uppercase()).collect());
        }

        let mut scored_words = Vec::new();

        for word_record in &possible_words {
            let word = word_record.word.to_uppercase();
            let score = self.calculate_word_score(&word, &possible_words, game_state).await;

            scored_words.push(WordScore {
                word: word.clone(),
                score,
                info_gain: score,
            });
        }

        scored_words.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

        Ok(scored_words.into_iter().take(10).map(|ws| ws.word).collect())
    }

    // 単語のスコアを計算（情報理論とヒューリスティックの組み合わせ）
    async fn calculate_word_score(&self, word: &str, possible_words: &[WordRecord], game_state: &GameState) -> f64 {
        let mut score = 0.0;

        // 1. 文字の多様性スコア
        let unique_chars: HashSet<char> = word.chars().collect();
        score += unique_chars.len() as f64 * 2.0;

        // 2. 頻出文字スコア（英語の一般的な文字頻度）
        let common_letters = "EAIOTRNSLCUDPMHGBFYWKVXZJQ";
        for ch in word.chars() {
            if let Some(pos) = common_letters.find(ch) {
                score += (26 - pos) as f64 * 0.1;
            }
        }

        // 3. 母音と子音のバランス
        let vowels = "AEIOU";
        let vowel_count = word.chars().filter(|&c| vowels.contains(c)).count();
        let consonant_count = word.len() - vowel_count;
        // 理想的なバランスに近いほど高スコア
        let balance_score = 5.0 - (vowel_count as f64 - 2.0).abs() - (consonant_count as f64 - 3.0).abs();
        score += balance_score.max(0.0);

        // 4. 既知の制約からの情報量
        let info_gain = self.calculate_information_gain(word, possible_words);
        score += info_gain;

        // 5. ゲームの進行に応じた重み調整
        let guess_count = game_state.guesses.len();
        if guess_count == 0 {
            // 最初の推測：多様性と一般的な文字を重視
            score += unique_chars.len() as f64 * 3.0;
        } else if guess_count >= 3 {
            // 後半：絞り込みを重視、情報ゲインを強化
            score += info_gain * 2.0;

            // 可能性の高い単語により高いスコアを与える
            if possible_words.len() <= 50 {
                score += 5.0;
            }
        }

        score
    }

    // 情報ゲイン（どれだけ候補を効率的に絞り込めるか）を計算
    fn calculate_information_gain(&self, word: &str, possible_words: &[WordRecord]) -> f64 {
        if possible_words.len() <= 1 {
            return 0.0;
        }

        let mut pattern_groups: HashMap<Vec<u8>, usize> = HashMap::new();

        for possible_word in possible_words {
            let pattern = self.simulate_guess_pattern(word, &possible_word.word.to_uppercase());
            *pattern_groups.entry(pattern).or_insert(0) += 1;
        }

        // エントロピーベースの情報ゲイン計算
        let total = possible_words.len() as f64;
        let mut entropy = 0.0;

        for &count in pattern_groups.values() {
            if count > 0 {
                let probability = count as f64 / total;
                entropy -= probability * probability.log2();
            }
        }

        // 最大エントロピーで正規化
        let max_entropy = (pattern_groups.len() as f64).log2();
        if max_entropy > 0.0 {
            entropy / max_entropy * 10.0 // スケーリング
        } else {
            0.0
        }
    }

    // 推測結果のパターンをシミュレート
    fn simulate_guess_pattern(&self, guess: &str, answer: &str) -> Vec<u8> {
        let guess_chars: Vec<char> = guess.chars().collect();
        let answer_chars: Vec<char> = answer.chars().collect();
        let mut pattern = vec![0u8; guess_chars.len()]; // 0: gray, 1: yellow, 2: green

        // まず緑を判定
        for i in 0..guess_chars.len() {
            if i < answer_chars.len() && guess_chars[i] == answer_chars[i] {
                pattern[i] = 2; // green
            }
        }

        // 次に黄色を判定
        let mut answer_counts: HashMap<char, usize> = HashMap::new();
        for (i, &ch) in answer_chars.iter().enumerate() {
            if i >= guess_chars.len() || guess_chars[i] != ch {
                *answer_counts.entry(ch).or_insert(0) += 1;
            }
        }

        for i in 0..guess_chars.len() {
            if pattern[i] == 0 { // まだ判定されていない
                let ch = guess_chars[i];
                if let Some(count) = answer_counts.get_mut(&ch) {
                    if *count > 0 {
                        pattern[i] = 1; // yellow
                        *count -= 1;
                    }
                }
            }
        }

        pattern
    }

    async fn get_letter_emoji(&self, letter: char, result: &LetterResult) -> String {
        let emoji_name = match result {
            LetterResult::Gray => format!("{}_gray", letter.to_ascii_lowercase()),
            LetterResult::Yellow => format!("{}_yellow", letter.to_ascii_lowercase()),
            LetterResult::Green => format!("{}_green", letter.to_ascii_lowercase()),
        };

        let cache = self.emoji_cache.read().await;
        if let Some(discord_format) = cache.get(&emoji_name) {
            discord_format.clone()
        } else {
            // フォールバック
            match result {
                LetterResult::Gray => format!("⬜{}", letter),
                LetterResult::Yellow => format!("🟨{}", letter),
                LetterResult::Green => format!("🟩{}", letter),
            }
        }
    }

    // ボタンのラベル用の絵文字を取得（標準の絵文字のみ）
    fn get_letter_emoji_for_button(&self, result: &LetterResult) -> String {
        match result {
            LetterResult::Gray => "⬜".to_string(),
            LetterResult::Yellow => "🟨".to_string(),
            LetterResult::Green => "🟩".to_string(),
        }
    }

    // 基本Embedを作成（初回のみ）
    fn create_base_embed() -> CreateEmbed {
        CreateEmbed::new()
            .title("🎯 Wordle Helper Tool")
            .color(Colour::BLUE)
    }

    // ゲーム状態に応じてEmbedの内容を更新
    async fn update_embed_content(&self, game_state: &GameState) -> String {
        if game_state.guesses.is_empty() && game_state.current_word.is_none() {
            "まだ推測がありません。新しい単語を入力してください！".to_string()
        } else {
            let mut description = String::new();

            // 過去の推測を表示
            for (i, guess) in game_state.guesses.iter().enumerate() {
                description.push_str(&format!("**{}回目:** ", i + 1));
                for (j, letter) in guess.word.chars().enumerate() {
                    if j < guess.results.len() {
                        let emoji = self.get_letter_emoji(letter, &guess.results[j]).await;
                        description.push_str(&emoji);
                    } else {
                        description.push_str(&format!("🔤{}", letter));
                    }
                }
                description.push('\n');
            }

            // 現在入力中の単語を表示
            if let Some(ref current_word) = game_state.current_word {
                description.push_str(&format!("\n**現在の単語:** "));
                for (i, letter) in current_word.chars().enumerate() {
                    if i < game_state.current_results.len() {
                        let emoji = self.get_letter_emoji(letter, &game_state.current_results[i]).await;
                        description.push_str(&emoji);
                    } else {
                        description.push_str(&format!("🔤{}", letter));
                    }
                }
                if game_state.pending_result {
                    description.push_str("\n⬇️ 各文字をクリックして色を変更し、確定ボタンを押してください");
                }
            }

            description
        }
    }

    fn create_result_buttons(&self, word: &str, current_results: &[LetterResult]) -> Vec<CreateActionRow> {
        let mut buttons = Vec::new();

        // 各文字のボタン（標準絵文字 + 文字表示）
        for (i, letter) in word.chars().enumerate() {
            let (emoji, style) = if i < current_results.len() {
                let emoji = self.get_letter_emoji_for_button(&current_results[i]);
                let style = match current_results[i] {
                    LetterResult::Gray => ButtonStyle::Secondary,
                    LetterResult::Yellow => ButtonStyle::Primary,
                    LetterResult::Green => ButtonStyle::Success,
                };
                (emoji, style)
            } else {
                (self.get_letter_emoji_for_button(&LetterResult::Gray), ButtonStyle::Secondary)
            };

            let button = CreateButton::new(format!("letter_{}_{}", i, letter))
                .label(format!("{} {}", emoji, letter))
                .style(style);
            buttons.push(button);
        }

        // 確定ボタン
        let confirm_button = CreateButton::new("confirm_result")
            .label("✅ 確定")
            .style(ButtonStyle::Success);
        buttons.push(confirm_button);

        // 5つずつのボタンで行を作成（Discordの制限）
        let mut rows = Vec::new();
        for chunk in buttons.chunks(5) {
            rows.push(CreateActionRow::Buttons(chunk.to_vec()));
        }

        rows
    }

    // 新しい単語入力ボタンを作成
    fn create_new_word_button(&self) -> Vec<CreateActionRow> {
        let button = CreateButton::new("new_word")
            .label("📝 新しい単語を入力")
            .style(ButtonStyle::Primary);

        vec![CreateActionRow::Buttons(vec![button])]
    }

    async fn suggest_words(&self, game_state: &GameState) -> String {
        match self.get_optimal_words(game_state).await {
            Ok(words) => {
                if words.is_empty() {
                    "候補となる単語が見つかりませんでした。制約を見直してください。".to_string()
                } else {
                    let mut suggestion = String::from("🎯 **おすすめの単語:**\n");

                    // 候補数の情報を先に表示
                    let possible_count = {
                        let all_words = self.word_cache.read().await;
                        self.filter_words_by_constraints(&all_words, game_state).len()
                    };
                    suggestion.push_str(&format!("💡 現在の候補数: **{}語**\n\n", possible_count));

                    // 単語リストを表示
                    for (i, word) in words.iter().enumerate() {
                        let medal = match i {
                            0 => "🥇",
                            1 => "🥈", 
                            2 => "🥉",
                            _ => "📝",
                        };
                        suggestion.push_str(&format!("{} **{}**\n", medal, word));

                        // 最初の5つまで表示
                        if i >= 4 {
                            break;
                        }
                    }

                    // 多くの候補がある場合はその旨を表示
                    if words.len() > 5 {
                        suggestion.push_str(&format!("... 他{}語\n", words.len() - 5));
                    }

                    suggestion
                }
            }
            Err(e) => {
                info!("Error getting optimal words: {:?}", e);
                "単語の提案を取得できませんでした。データベースの接続を確認してください。".to_string()
            }
        }
    }
}

#[async_trait]
impl EventHandler for Bot {
    async fn ready(&self, ctx: Context, ready: Ready) {
        info!("{} is connected!", ready.user.name);

        // 絵文字キャッシュを読み込み
        if let Err(e) = self.load_emoji_cache().await {
            info!("Failed to load emoji cache: {:?}", e);
        } else {
            let emoji_count = self.emoji_cache.read().await.len();
            info!("Successfully loaded {} emojis", emoji_count);
        }

        // 単語キャッシュを読み込み
        if let Err(e) = self.load_word_cache().await {
            info!("Failed to load word cache: {:?}", e);
            info!("Will use fallback words for suggestions");
        } else {
            let word_count = self.word_cache.read().await.len();
            info!("Successfully loaded {} words", word_count);
        }

        let commands = vec![
            CreateCommand::new("ping").description("Pong"),
            CreateCommand::new("wht").description("Wordle Helper Tool"),
        ];
        let commands = &self.discord_guild_id.set_commands(&ctx.http, commands).await.unwrap();

        info!("Registered commands: {:#?}", commands);
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        match interaction {
            Interaction::Command(command) => {
                match command.data.name.as_str() {
                    "ping" => {
                        let data = CreateInteractionResponseMessage::new().content("Pong");
                        let builder = CreateInteractionResponse::Message(data);

                        if let Err(why) = command.create_response(&ctx.http, builder).await {
                            println!("Cannot respond to slash command: {why}");
                        }
                    }
                    "wht" => {
                        let user_id = command.user.id.get();

                        // ゲーム状態を初期化（基本Embedも含む）
                        {
                            let mut states = self.game_states.write().await;
                            states.insert(user_id, GameState {
                                guesses: Vec::new(),
                                current_word: None,
                                pending_result: false,
                                current_results: Vec::new(),
                                last_suggestion: String::new(),
                            });
                        }

                        // 初期表示用の埋め込みを作成
                        let embed = Self::create_base_embed()
                            .description("まだ推測がありません。新しい単語を入力してください！");

                        // 新しい単語入力ボタンを追加
                        let components = self.create_new_word_button();

                        let response = CreateInteractionResponseMessage::new()
                            .embed(embed)
                            .components(components);

                        let builder = CreateInteractionResponse::Message(response);

                        if let Err(why) = command.create_response(&ctx.http, builder).await {
                            println!("Cannot respond to slash command: {why}");
                        }
                    }
                    command => unreachable!("Unknown command: {}", command),
                }
            }
            Interaction::Modal(modal) => {
                self.handle_modal_interaction(ctx, modal).await;
            }
            Interaction::Component(component) => {
                self.handle_component_interaction(ctx, component).await;
            }
            _ => {}
        }
    }
}

impl Bot {
    async fn handle_modal_interaction(&self, ctx: Context, modal: ModalInteraction) {
        if modal.data.custom_id == "word_input_modal" {
            let word = if let Some(row) = modal.data.components.first() {
                if let Some(component) = row.components.first() {
                    match component {
                        serenity::all::ActionRowComponent::InputText(input) => {
                            input.value.clone().unwrap_or_default().to_uppercase()
                        }
                        _ => "ERROR".to_string(),
                    }
                } else {
                    "ERROR".to_string()
                }
            } else {
                "ERROR".to_string()
            };

            let user_id = modal.user.id.get();

            // ゲーム状態を更新
            {
                let mut states = self.game_states.write().await;
                if let Some(state) = states.get_mut(&user_id) {
                    state.current_word = Some(word.clone());
                    state.pending_result = true;
                    // 初期状態は全て灰色
                    state.current_results = vec![LetterResult::Gray; word.len()];
                }
            }

            // 現在の状態を表示
            let (embed, components) = {
                let states = self.game_states.read().await;
                if let Some(state) = states.get(&user_id) {
                    let description = self.update_embed_content(state).await;
                    let embed = Self::create_base_embed().description(description);

                    let components = if state.pending_result {
                        self.create_result_buttons(&word, &state.current_results)
                    } else {
                        Vec::new()
                    };

                    (embed, components)
                } else {
                    (Self::create_base_embed().description("エラーが発生しました"), Vec::new())
                }
            };

            let mut response = CreateInteractionResponseMessage::new()
                .embed(embed);

            if !components.is_empty() {
                response = response.components(components);
            }

            // ここが重要：UpdateMessageを使用してEmbedを更新（新しいメッセージを作らない）
            let builder = CreateInteractionResponse::UpdateMessage(response);

            if let Err(why) = modal.create_response(&ctx.http, builder).await {
                println!("Cannot respond to modal: {why}");
            }
        }
    }

    async fn handle_component_interaction(&self, ctx: Context, component: ComponentInteraction) {
        let user_id = component.user.id.get();

        if component.data.custom_id == "new_word" {
            // 新しい単語入力モーダルを表示
            let word_input = CreateInputText::new(InputTextStyle::Short, "word", "単語を入力")
                .placeholder("5文字の英単語を入力してください")
                .min_length(5)
                .max_length(5)
                .required(true);

            let modal = CreateModal::new("word_input_modal", "単語を入力")
                .components(vec![CreateActionRow::InputText(word_input)]);

            let response = CreateInteractionResponse::Modal(modal);

            if let Err(why) = component.create_response(&ctx.http, response).await {
                println!("Cannot respond to component: {why}");
            }
        } else if component.data.custom_id == "confirm_result" {
            let loading_embed = Self::create_base_embed()
                .description("⏳ 最適な単語を分析中...");
            
            let loading_response = CreateInteractionResponseMessage::new()
                .embed(loading_embed)
                .components(self.create_new_word_button());
            
            let update_response = CreateInteractionResponse::UpdateMessage(loading_response);
            
            if let Err(why) = component.create_response(&ctx.http, update_response).await {
                println!("Cannot respond to component: {why}");
                return;
            }

            // 時間のかかる処理を非同期で実行
            let (embed, components) = {
                let mut states = self.game_states.write().await;
                if let Some(state) = states.get_mut(&user_id) {
                    if let Some(current_word) = &state.current_word {
                        // 現在の結果を履歴に追加
                        let guess = WordleGuess {
                            word: current_word.clone(),
                            results: state.current_results.clone(),
                        };
                        state.guesses.push(guess);

                        // 状態をリセット
                        state.current_word = None;
                        state.pending_result = false;
                        state.current_results.clear();
                    }

                    // まず基本的な情報を表示（提案は後で）
                    let basic_description = self.update_embed_content(state).await;
                    let embed = Self::create_base_embed()
                        .description(format!("{}\n\n⏳ 最適な単語を分析中...", basic_description));
                    let components = self.create_new_word_button();

                    (embed, components)
                } else {
                    let embed = Self::create_base_embed().description("ゲーム状態が見つかりません。");
                    (embed, Vec::new())
                }
            };

            // まずローディング状態を表示（既存のメッセージを更新）
            let loading_response = EditInteractionResponse::new()
                .embed(embed)
                .components(components);

            if let Err(why) = component.edit_response(&ctx.http, loading_response).await {
                println!("Cannot edit response: {why}");
                return;
            }

            // バックグラウンドで単語提案を生成
            let ctx_clone = ctx.clone();
            let component_clone = component.clone();
            let bot_clone = Bot {
                client: self.client.clone(),
                discord_guild_id: self.discord_guild_id,
                supabase_url: self.supabase_url.clone(),
                supabase_key: self.supabase_key.clone(),
                game_states: Arc::clone(&self.game_states),
                emoji_cache: Arc::clone(&self.emoji_cache),
                word_cache: Arc::clone(&self.word_cache),
            };

            tokio::spawn(async move {
                // 単語提案を生成
                let suggestion = {
                    let states = bot_clone.game_states.read().await;
                    if let Some(state) = states.get(&user_id) {
                        bot_clone.suggest_words(state).await
                    } else {
                        "ゲーム状態が見つかりません。".to_string()
                    }
                };

                // 最終的な表示を更新（既存のメッセージを更新）
                let (final_embed, final_components) = {
                    let mut states = bot_clone.game_states.write().await;
                    if let Some(state) = states.get_mut(&user_id) {
                        state.last_suggestion = suggestion.clone();

                        let description = format!("{}\n\n{}", 
                            bot_clone.update_embed_content(state).await,
                            suggestion
                        );
                        let embed = Bot::create_base_embed().description(description);
                        let components = bot_clone.create_new_word_button();

                        (embed, components)
                    } else {
                        let embed = Bot::create_base_embed().description("ゲーム状態が見つかりません。");
                        (embed, Vec::new())
                    }
                };

                let final_response = EditInteractionResponse::new()
                    .embed(final_embed)
                    .components(final_components);

                if let Err(why) = component_clone.edit_response(&ctx_clone.http, final_response).await {
                    println!("Cannot edit final response: {why}");
                }
            });

        } else if component.data.custom_id.starts_with("letter_") {
            let parts: Vec<&str> = component.data.custom_id.split('_').collect();

            if parts.len() >= 2 {
                if let Ok(index) = parts[1].parse::<usize>() {
                    let (embed, components) = {
                        let mut states = self.game_states.write().await;
                        if let Some(state) = states.get_mut(&user_id) {
                            if index < state.current_results.len() {
                                // 状態を循環させる: Gray -> Yellow -> Green -> Gray
                                state.current_results[index] = match state.current_results[index] {
                                    LetterResult::Gray => LetterResult::Yellow,
                                    LetterResult::Yellow => LetterResult::Green,
                                    LetterResult::Green => LetterResult::Gray,
                                };
                            }

                            let description = self.update_embed_content(state).await;
                            let embed = Self::create_base_embed().description(description);
                            let components = if let Some(ref word) = state.current_word {
                                self.create_result_buttons(word, &state.current_results)
                            } else {
                                Vec::new()
                            };

                            (embed, components)
                        } else {
                            (Self::create_base_embed().description("ゲーム状態が見つかりません。"), Vec::new())
                        }
                    };

                    let mut response = CreateInteractionResponseMessage::new()
                        .embed(embed);

                    if !components.is_empty() {
                        response = response.components(components);
                    }

                    // UpdateMessage を使って既存のメッセージを更新
                    if let Err(why) = component.create_response(&ctx.http, CreateInteractionResponse::UpdateMessage(response)).await {
                        println!("Cannot respond to component: {why}");
                    }
                } else {
                    // エラーメッセージは一時的に表示（ephemeral）
                    let response = CreateInteractionResponseMessage::new()
                        .content("エラーが発生しました")
                        .ephemeral(true);

                    if let Err(why) = component.create_response(&ctx.http, CreateInteractionResponse::Message(response)).await {
                        println!("Cannot respond to component: {why}");
                    }
                }
            }
        }
    }
}

#[shuttle_runtime::main]
async fn serenity(
    #[shuttle_runtime::Secrets] secret_store: SecretStore,
) -> shuttle_serenity::ShuttleSerenity {
    let discord_token = secret_store
        .get("DISCORD_TOKEN")
        .context("'DISCORD_TOKEN' was not found")?;

    let discord_guild_id = secret_store
        .get("DISCORD_GUILD_ID")
        .context("'DISCORD_GUILD_ID' was not found")?;

    let supabase_url = secret_store
        .get("SUPABASE_URL")
        .context("'SUPABASE_URL' was not found")?;

    let supabase_key = secret_store
        .get("SUPABASE_KEY")
        .context("'SUPABASE_KEY' was not found")?;

    let client = get_client(
        &discord_token,
        discord_guild_id.parse().unwrap(),
        supabase_url,
        supabase_key,
    )
    .await;
    Ok(client.into())
}

pub async fn get_client(
    discord_token: &str,
    discord_guild_id: u64,
    supabase_url: String,
    supabase_key: String,
) -> Client {
    let intents = GatewayIntents::empty();

    Client::builder(discord_token, intents)
        .event_handler(Bot {
            client: reqwest::Client::new(),
            discord_guild_id: GuildId::new(discord_guild_id),
            supabase_url,
            supabase_key,
            game_states: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            emoji_cache: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            word_cache: Arc::new(tokio::sync::RwLock::new(Vec::new())),
        })
        .await
        .expect("Error creating client")
}