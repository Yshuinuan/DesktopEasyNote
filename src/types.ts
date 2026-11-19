/**
 * 共享类型：与 Rust 侧 serde 序列化结构一一对应。
 * 契约见 docs/开发契约.md「TS 类型」一节；P1 只读引用，不要改字段名。
 *
 * v1.9 R16：心情改为任意字符串，默认/自定义/隐藏由 Rust `get_moods()` 提供，
 * 前端不再硬编码六动词。
 */

/** 心情：默认六词或用户自定义词（≤8 字），由 Rust 数据层校验。 */
export type Mood = string;

/** get_moods / add_mood / delete_mood 的返回视图。 */
export interface MoodsView {
  /** 菜单可见顺序：默认在前，自定义按添加顺序。 */
  visible: string[];
  /** 默认词（固定顺序，可能含被隐藏的）。 */
  defaults: string[];
  /** 自定义词（按添加顺序，可能含被隐藏的）。 */
  custom: string[];
  /** 被删除（仅隐藏）的词，可能含默认词。 */
  hidden: string[];
}

/** R17：一条预设标题（level 1–6 + 文字，由 Rust 数据层校验）。 */
export interface HeadingPreset {
  level: number;
  text: string;
}

/** 一条记录，对应 data/YYYY-MM-DD.md 的一行。 */
export interface Entry {
  /** 本地时间 HH:MM，例：08:47 */
  time: string;
  /** 内容，可含 " · " */
  content: string;
  /** 心情，可省略 -> null；任意已配置心情词 */
  mood: Mood | null;
}

/** get_settings 的返回，永不包含 key 明文。 */
export interface SettingsView {
  has_api_key: boolean;
  base_url: string;
  model: string;
}
