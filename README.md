# splicedown

`splicedown` は、Rust で書かれた、Rust のための高速でモダンなバンドラーです。

## Why splicedown?

`cargo-equip` のような既存のバンドラーは、Edition 2018 等の古い Edition の Rust コードのみをサポートしており、Edition 2024
に対応したバンドラーはほとんどありません。  
そこで、`splicedown` は最新の Rust Edition 2024 に対応したバンドラーとして開発されました。

## Features

- Edition 2024 対応
    - 現在は Edition 2024 のみをサポートしています。
    - 将来的に、古い Edition のクレートを `cargo fix --edition` を使用して自動的に Edition 2024
      に変換し、そこからバンドルを行う機能を追加する予定です。
- 出力コードの最適化
    - minify 機能を利用して、安全にツリーシェイキングを行うことができます。
    - バンドル結果で使われることの少ないテストコード (`#[cfg(test)]` などの属性がついているコード)
      を削除することも可能です。これにより、バンドル結果のサイズを大幅に削減することができます。

## Installation

```bash
cargo binstall --locked splicedown # 通常の cargo install でも可
# or
cargo binstall --locked --git https://github.com/Yu-yu0202/splicedown.git
```

## Usage

```bash
splicedown <entry_file> [--manifest-path <PATH>] [--output <PATH>] [--exclude <CRATE(s)>] [--exclude-preset <PRESET(s)>] [--(no-)minify] [--(no-)minify-test] [--no-check] [--keep-check-dir]
```

- `<entry_file>`: バンドルのエントリーポイントとなる Rust ファイルのパスを指定します。
    - デフォルトは `src/main.rs` です。
- `--manifest-path <PATH>`: Cargo.toml のパスを指定します。
    - 指定されなかった場合、 `<entry_file>` のディレクトリから上方向に探索します。
- `--output <PATH>`: バンドル結果の出力先を指定します。
    - 指定されなかった場合、標準出力に出力されます。
- `--exclude <CRATE(s)>`: バンドルから除外するクレートを指定します。
    - 反復可能です。`--exclude crate1 --exclude crate2` のように複数指定できます。
- `--exclude-preset <PRESET(s)>`: バンドルから除外するクレートのプリセットを指定します。
    - 反復可能です。`--exclude-preset preset1 --exclude-preset preset2` のように複数指定できます。
    - 現在は `atcoder-2025-10` / `atcoder-2025` のみがサポートされています。
    - プリセットで除外されるクレートについては、[EX01: exclude presetについて](docs/EX01_exclude-presets.md) を参照してください。
- `--(no-)minify`: バンドル結果の最適化を有効/無効にします。
    - デフォルトは有効です。
    - `--minify` を指定すると、`--minify-test` も有効になります。
    - `--no-minify` を指定すると、最適化を無効にすることができます。同時に `--no-minify-test` も有効になります。
- `--(no-)minify-test`: バンドル結果からテストコードを削除するかどうかを指定します。
    - デフォルトは有効です。
    - `--no-minify-test` を指定すると、テストコードを削除しないようにすることができます。
- `--no-check`: バンドル前の `cargo check` の実行をスキップします。
    - デフォルトは無効です。
    - 有効でも、minify 時の安全性チェックとして行われる check はスキップされません。
- `--keep-check-dir`: バンドル前の `cargo check` の実行時に生成される一時ディレクトリを削除せずに残すかどうかを指定します。
    - デフォルトは無効です。

## License

MIT License

---

おまけ

- 名前の `splicedown` は、splice (結合する) と down (下・`rolldown` 的な命名) から来ています。特にそれ以上でもそれ以下でもないです。
