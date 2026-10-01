-- ============================================================================
-- test 库种子数据：表结构 + 测试数据，供本地与 CI 手动探索使用。
--
-- 用法（容器需已启动）：
--   docker compose up -d --wait
--   docker cp scripts/seed-test-db.sql mssql-test:/seed.sql
--   docker exec mssql-test /opt/mssql-tools18/bin/sqlcmd \
--     -S localhost -U sa -P 'YourStrong!Passw0rd' -C -i /seed.sql
--
-- 设计目标：让 MCP 服务器的每条代码路径都有数据可打——
--   * 各类标量类型（money/datetime2/datetimeoffset/time/varbinary/uniqueidentifier/float/real/bit）
--   * NULL、中文、含逗号/引号/换行的值（验证 CSV/JSON/Markdown 转义）
--   * 复合主键、带 INCLUDE 列的索引、外键
--   * 视图（含聚合），供 describe_table 的 dependent-views 分节
--   * 列级 collation 混用，供 COLLATION_NAME 分节
-- 幂等：重复执行安全。
-- ============================================================================

IF DB_ID('test') IS NULL CREATE DATABASE test;
GO
USE test;
GO

-- 先删视图（视图依赖表，必须先于表删除）
IF OBJECT_ID('dbo.v_order_summary','V') IS NOT NULL DROP VIEW dbo.v_order_summary;
IF OBJECT_ID('dbo.v_active_users','V')  IS NOT NULL DROP VIEW dbo.v_active_users;
GO
-- 再删表：顺序满足外键依赖（子表在前）
IF OBJECT_ID('dbo.order_items','U')   IS NOT NULL DROP TABLE dbo.order_items;
IF OBJECT_ID('dbo.orders','U')        IS NOT NULL DROP TABLE dbo.orders;
IF OBJECT_ID('dbo.products','U')      IS NOT NULL DROP TABLE dbo.products;
IF OBJECT_ID('dbo.categories','U')    IS NOT NULL DROP TABLE dbo.categories;
IF OBJECT_ID('dbo.users','U')         IS NOT NULL DROP TABLE dbo.users;
IF OBJECT_ID('dbo.collation_demo','U') IS NOT NULL DROP TABLE dbo.collation_demo;
GO

-- ---------------------------------------------------------------------------
-- 用户：覆盖常见标量类型与 NULL
-- ---------------------------------------------------------------------------
CREATE TABLE dbo.users (
    id           INT IDENTITY(1,1) CONSTRAINT PK_users PRIMARY KEY,
    username     NVARCHAR(50)   NOT NULL CONSTRAINT UQ_users_username UNIQUE,
    display_name NVARCHAR(100)  NULL,
    email        VARCHAR(120)   NULL,
    age          TINYINT        NULL,
    balance      MONEY          NULL,
    is_active    BIT            NOT NULL CONSTRAINT DF_users_active DEFAULT (1),
    created_at   DATETIME2(3)   NOT NULL,
    last_login   DATETIME       NULL,
    birthday     DATE           NULL,
    login_time   TIME(3)        NULL,
    api_key      UNIQUEIDENTIFIER NULL,
    avatar       VARBINARY(MAX) NULL,
    score        FLOAT          NULL,
    rating       REAL           NULL
);
GO

INSERT INTO dbo.users
  (username, display_name, email, age, balance, is_active, created_at, last_login, birthday, login_time, api_key, avatar, score, rating)
VALUES
  ('zhang_san',   N'张三', 'zhangsan@example.com', 28, 1234.5678, 1, '2023-01-15T09:30:00.123', '2024-05-01T14:22:33.000', '1996-03-12', '09:30:15.250', '11111111-1111-1111-1111-111111111111', 0xDEADBEEF, 88.5,  4.5),
  ('li_si',       N'李四', 'lisi@example.com',     34, 98765.4321, 1, '2023-02-20T10:00:00.000', '2024-06-11T08:05:00.000', '1990-07-21', '23:59:59.997', '22222222-2222-2222-2222-222222222222', NULL,       17976931348623157, 3.4),
  ('wang_wu',     N'王五', NULL,                   NULL, NULL,      0, '2023-03-01T00:00:00.000', NULL, NULL, NULL, NULL, NULL, NULL, NULL),
  ('zhao_liu',    N'赵六', 'zhaoliu@example.com',  41, 0.0000,    1, '2023-04-10T12:00:00.000', '2024-01-02T06:30:00.000', '1983-11-30', '06:30:00.000', '33333333-3333-3333-3333-333333333333', 0x00,       -0.25, -1.5),
  -- 特殊字符：覆盖 CSV 引号包裹 / JSON 无歧义 / Markdown 转义
  ('edge_cases',  N'含"引号"、逗号，和
换行的名字', 'edge@example.com', 30, -99.9900, 1, '2023-05-05T05:05:05.005', '2024-03-03T03:03:03.000', '1994-05-05', '03:03:03.003', '44444444-4444-4444-4444-444444444444', 0xCAFEBABE, 1e16, 0.0001),
  ('sun_qi',      N'孙七', 'sunqi@example.com',    25, 500.0000,  1, '2023-06-18T18:20:00.000', '2024-07-19T19:20:00.000', '1999-06-18', '18:20:00.000', NULL, NULL, 72.25, 4.0),
  ('zhou_ba',     N'周八', NULL,                   52, 120000.0000, 0, '2023-07-22T08:00:00.000', NULL, '1972-02-02', NULL, NULL, NULL, NULL, NULL),
  ('wu_jiu',      N'吴九', 'wujiu@example.com',    19, 88.8800,   1, '2023-08-30T21:45:00.000', '2024-08-15T21:45:00.000', '2005-08-30', '21:45:10.500', '55555555-5555-5555-5555-555555555555', NULL, 15.75, 2.5),
  ('zheng_shi',   N'郑十', 'zhengshi@example.com', 63, 7.7777,    1, '2023-09-09T09:09:09.009', '2024-09-09T09:09:09.000', '1961-09-09', '09:09:09.009', '66666666-6666-6666-6666-666666666666', 0x01, 3.14, 3.1),
  ('admin',       N'管理员', 'admin@example.com',  37, 999999.9999, 1, '2023-01-01T00:00:00.000', '2024-10-01T10:10:10.000', '1987-01-01', '10:10:10.100', '77777777-7777-7777-7777-777777777777', 0xFFEE, 100.0, 5.0);
GO

-- ---------------------------------------------------------------------------
-- 分类 / 商品 / 订单 / 订单明细（外键 + 复合主键 + INCLUDE 索引）
-- ---------------------------------------------------------------------------
CREATE TABLE dbo.categories (
    id   INT           NOT NULL CONSTRAINT PK_categories PRIMARY KEY,
    name NVARCHAR(50)  NOT NULL
);
GO

CREATE TABLE dbo.products (
    id          INT IDENTITY(1,1) CONSTRAINT PK_products PRIMARY KEY,
    name        NVARCHAR(100)  NOT NULL,
    category_id INT            NULL CONSTRAINT FK_products_category REFERENCES dbo.categories(id),
    price       DECIMAL(10,2)  NOT NULL,
    stock       INT            NOT NULL CONSTRAINT DF_products_stock DEFAULT (0),
    description NVARCHAR(MAX)  NULL,
    created_at  DATETIMEOFFSET(3) NOT NULL
);
GO
-- 复合索引 + INCLUDE 列：验证 describe_table 的索引分节
CREATE INDEX IX_products_category ON dbo.products (category_id) INCLUDE (price, stock);
GO

CREATE TABLE dbo.orders (
    id           INT IDENTITY(1,1) CONSTRAINT PK_orders PRIMARY KEY,
    user_id      INT            NOT NULL CONSTRAINT FK_orders_user REFERENCES dbo.users(id),
    status       NVARCHAR(20)   NOT NULL,
    total_amount DECIMAL(12,2)  NULL,
    note         NVARCHAR(500)  NULL,
    order_date   DATETIME2(3)   NOT NULL
);
GO
CREATE INDEX IX_orders_user_date ON dbo.orders (user_id, order_date DESC);
GO

-- 复合主键：验证索引列各自成行，而非被逗号拼进一个字段
CREATE TABLE dbo.order_items (
    order_id   INT           NOT NULL CONSTRAINT FK_items_order REFERENCES dbo.orders(id),
    line_no    INT           NOT NULL,
    product_id INT           NOT NULL CONSTRAINT FK_items_product REFERENCES dbo.products(id),
    quantity   INT           NOT NULL,
    unit_price DECIMAL(10,2) NOT NULL,
    CONSTRAINT PK_order_items PRIMARY KEY (order_id, line_no)
);
GO

-- 列级 collation 混用：验证 COLLATION_NAME 逐列输出
CREATE TABLE dbo.collation_demo (
    id          INT          NOT NULL CONSTRAINT PK_collation_demo PRIMARY KEY,
    latin_col   VARCHAR(20)  COLLATE Latin1_General_CI_AS   NULL,
    chinese_col NVARCHAR(20) COLLATE Chinese_PRC_90_CI_AS   NULL,
    plain_int   INT          NULL
);
GO

-- ---------------------------------------------------------------------------
-- 数据
-- ---------------------------------------------------------------------------
INSERT INTO dbo.categories (id, name) VALUES
  (1, N'电子产品'), (2, N'图书'), (3, N'家居'), (4, N'服装'), (5, N'食品'), (6, N'运动户外');
GO

INSERT INTO dbo.products (name, category_id, price, stock, description, created_at) VALUES
  (N'机械键盘',     1, 399.00,  25, N'青轴，手感清脆',                       '2024-01-05T10:00:00.000+08:00'),
  (N'无线鼠标',     1, 199.50,  80, N'静音微动，续航 6 个月',                 '2024-01-06T10:00:00.000+08:00'),
  (N'4K 显示器',    1, 2499.99, 12, N'27 英寸，IPS 面板',                     '2024-01-07T10:00:00.000+08:00'),
  (N'深入理解计算机系统', 2, 139.00, 40, N'CSAPP 第三版，含"练习题"答案',         '2024-02-01T09:00:00.000+08:00'),
  (N'Rust 程序设计', 2, 118.00,  33, N'平装，528 页',                         '2024-02-11T09:00:00.000+08:00'),
  (N'实木书架',     3, 599.00,   8, N'三层，可调节层板',                      '2024-03-01T11:00:00.000+08:00'),
  (N'香薰蜡烛',     3,  68.00, 150, N'大豆蜡，燃烧约 40 小时
含广口玻璃杯',      '2024-03-05T11:00:00.000+08:00'),
  (N'纯棉T恤',      4,  89.00, 200, N'100% 棉，黑白灰三色',                   '2024-04-01T14:00:00.000+08:00'),
  (N'冲锋衣',       4, 899.00,  45, N'三合一，防风防水',                      '2024-04-15T14:00:00.000+08:00'),
  (N'挂耳咖啡',     5,  45.00, 300, N'中度烘焙，10 片装',                     '2024-05-01T08:00:00.000+08:00'),
  (N'黑巧克力',     5,  32.50, 180, N'可可含量 85%',                          '2024-05-20T08:00:00.000+08:00'),
  (N'瑜伽垫',       6, 129.00,  60, N'TPE 材质，6mm 厚',                      '2024-06-01T16:00:00.000+08:00'),
  (N'跳绳',         6,  29.90, 120, N'轴承钢丝，可调节长度',                  '2024-06-10T16:00:00.000+08:00'),
  (N'哑铃',         6, 259.00,  35, N'包胶，一对 10kg',                       '2024-06-20T16:00:00.000+08:00'),
  (N'断货商品',     1, 999.00,   0, N'用于测试 stock = 0',                    '2024-07-01T10:00:00.000+08:00'),
  (N'NULL 描述商品', 2,  55.00,  10, NULL,                                     '2024-07-15T10:00:00.000+08:00');
GO

INSERT INTO dbo.orders (user_id, status, total_amount, note, order_date) VALUES
  (1, N'已完成',  598.50, N'加急',                 '2024-01-10T09:15:00.000'),
  (1, N'待发货',  139.00, NULL,                    '2024-02-02T10:30:00.000'),
  (2, N'已完成', 2499.99, N'发票抬头：张三科技有限公司', '2024-02-14T15:45:00.000'),
  (2, N'已取消',   45.00, N'不想要了',             '2024-03-03T11:00:00.000'),
  (4, N'已完成',  118.00, NULL,                    '2024-03-18T20:20:00.000'),
  (5, N'待付款',   68.00, N'备注含"引号"与,逗号',  '2024-04-04T12:00:00.000'),
  (6, N'已完成',   89.00, NULL,                    '2024-04-20T09:00:00.000'),
  (6, N'已发货',  928.00, N'分两个包裹发',          '2024-05-05T13:13:00.000'),
  (8, N'已完成',   32.50, NULL,                    '2024-05-25T17:40:00.000'),
  (9, N'已完成',  129.00, N'送人',                 '2024-06-06T08:08:00.000'),
  (9, N'待发货',   29.90, NULL,                    '2024-06-16T19:19:00.000'),
  (10, N'已完成', 259.00, N'企业采购',             '2024-07-07T10:10:00.000'),
  (10, N'已完成',  999.00, NULL,                    '2024-07-17T11:11:00.000'),
  (3, N'已取消',   55.00, NULL,                    '2024-08-08T22:22:00.000'),
  (7, N'已完成',   77.70, N'老客户',               '2024-09-09T09:09:00.000');
GO

INSERT INTO dbo.order_items (order_id, line_no, product_id, quantity, unit_price) VALUES
  (1,1,1,1,399.00),(1,2,2,1,199.50),
  (2,1,4,1,139.00),
  (3,1,3,1,2499.99),
  (4,1,10,1,45.00),
  (5,1,5,1,118.00),
  (6,1,7,1,68.00),
  (7,1,8,1,89.00),
  (8,1,9,1,899.00),(8,2,8,1,29.00),
  (9,1,11,1,32.50),
  (10,1,12,1,129.00),
  (11,1,13,1,29.90),
  (12,1,14,1,259.00),
  (13,1,15,1,999.00),
  (14,1,16,1,55.00),
  (15,1,10,1,45.00),(15,2,11,1,32.70);
GO

INSERT INTO dbo.collation_demo (id, latin_col, chinese_col, plain_int) VALUES
  (1, 'Apple',  N'苹果', 1),
  (2, 'banana', N'香蕉', 2),
  (3, NULL,     NULL,    3);
GO

-- ---------------------------------------------------------------------------
-- 视图：供 describe_table 的 DEPENDENT_VIEWS 分节与只读查询使用
-- ---------------------------------------------------------------------------
CREATE VIEW dbo.v_order_summary AS
SELECT
    o.id            AS order_id,
    u.username      AS username,
    o.status        AS status,
    o.order_date    AS order_date,
    COUNT(oi.line_no)               AS line_count,
    ISNULL(SUM(oi.quantity), 0)     AS total_qty,
    ISNULL(SUM(oi.quantity * oi.unit_price), 0) AS total_amount
FROM dbo.orders o
JOIN dbo.users u ON u.id = o.user_id
LEFT JOIN dbo.order_items oi ON oi.order_id = o.id
GROUP BY o.id, u.username, o.status, o.order_date;
GO

CREATE VIEW dbo.v_active_users AS
SELECT id, username, display_name, email, balance, created_at
FROM dbo.users
WHERE is_active = 1;
GO

PRINT 'seed done: test 库已就绪';
GO
