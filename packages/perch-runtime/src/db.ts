import { connect, parseConnectionString, type Connection } from "@perry/postgres/src/index";

let conn: Connection | null = null;

async function getConnection(): Promise<Connection> {
  if (conn !== null) return conn;

  const url = process.env.PERCH_DB_URL;
  if (!url || url === "") {
    throw new Error(
      "PERCH_DB_URL not set. Configure Postgres in runtime.toml " +
      "[postgres] url and make sure perch-worker sees it as an env var."
    );
  }

  conn = await connect(parseConnectionString(url));
  return conn;
}

// ──────────────────────────────────────────────────────────────────────
// QueryBuilder
// ──────────────────────────────────────────────────────────────────────

class QueryBuilder {
  private _table: string = "";
  private _wheres: Array<{ field: string; op: string; value: any }> = [];
  private _limit: number | null = null;
  private _offset: number | null = null;
  private _orderBy: string | null = null;
  private _orderDir: string = "ASC";
  private _selects: string[] = [];
  private _joins: string[] = [];
  private _groupBy: string | null = null;

  table(name: string): QueryBuilder {
    const qb = new QueryBuilder();
    qb._table = name;
    return qb;
  }

  where(conditions: Record<string, any>): QueryBuilder {
    const keys = Object.keys(conditions);
    for (let i = 0; i < keys.length; i++) {
      const key = keys[i];
      const val = conditions[key];
      if (typeof val === "object" && val !== null) {
        const ops = Object.keys(val);
        for (let j = 0; j < ops.length; j++) {
          const op = ops[j];
          const opVal = val[op];
          let sqlOp = "=";
          if (op === "gt") sqlOp = ">";
          else if (op === "gte") sqlOp = ">=";
          else if (op === "lt") sqlOp = "<";
          else if (op === "lte") sqlOp = "<=";
          else if (op === "ne") sqlOp = "!=";
          else if (op === "like") sqlOp = "LIKE";
          this._wheres.push({ field: key, op: sqlOp, value: opVal });
        }
      } else {
        this._wheres.push({ field: key, op: "=", value: val });
      }
    }
    return this;
  }

  select(...fields: string[]): QueryBuilder {
    this._selects = fields;
    return this;
  }

  join(table: string, left: string, right: string): QueryBuilder {
    this._joins.push("JOIN " + table + " ON " + left + " = " + right);
    return this;
  }

  groupBy(field: string): QueryBuilder {
    this._groupBy = field;
    return this;
  }

  orderBy(field: string, dir: string = "ASC"): QueryBuilder {
    this._orderBy = field;
    this._orderDir = dir.toUpperCase();
    return this;
  }

  limit(n: number): QueryBuilder {
    this._limit = n;
    return this;
  }

  offset(n: number): QueryBuilder {
    this._offset = n;
    return this;
  }

  private buildSelect(): { sql: string; params: any[] } {
    const fields = this._selects.length > 0 ? this._selects.join(", ") : "*";
    let sql = "SELECT " + fields + " FROM " + this._table;

    if (this._joins.length > 0) {
      sql += " " + this._joins.join(" ");
    }

    const params: any[] = [];
    if (this._wheres.length > 0) {
      const clauses: string[] = [];
      for (let i = 0; i < this._wheres.length; i++) {
        const w = this._wheres[i];
        params.push(w.value);
        clauses.push(w.field + " " + w.op + " $" + params.length);
      }
      sql += " WHERE " + clauses.join(" AND ");
    }

    if (this._groupBy) sql += " GROUP BY " + this._groupBy;
    if (this._orderBy) sql += " ORDER BY " + this._orderBy + " " + this._orderDir;
    if (this._limit !== null) sql += " LIMIT " + this._limit;
    if (this._offset !== null) sql += " OFFSET " + this._offset;

    return { sql, params };
  }

  async all(): Promise<any[]> {
    const c = await getConnection();
    const { sql, params } = this.buildSelect();
    const result = await c.query(sql, params);
    return result.rows;
  }

  async first(): Promise<any | null> {
    this._limit = 1;
    const rows = await this.all();
    return rows.length > 0 ? rows[0] : null;
  }

  async insert(data: Record<string, any>): Promise<void> {
    const keys = Object.keys(data);
    const placeholders: string[] = [];
    const params: any[] = [];
    for (let i = 0; i < keys.length; i++) {
      params.push(data[keys[i]]);
      placeholders.push("$" + (i + 1));
    }
    const sql =
      "INSERT INTO " + this._table +
      " (" + keys.join(", ") + ")" +
      " VALUES (" + placeholders.join(", ") + ")";
    const c = await getConnection();
    await c.query(sql, params);
  }

  async update(data: Record<string, any>): Promise<void> {
    const setKeys = Object.keys(data);
    const params: any[] = [];
    const setClauses: string[] = [];
    for (let i = 0; i < setKeys.length; i++) {
      params.push(data[setKeys[i]]);
      setClauses.push(setKeys[i] + " = $" + params.length);
    }

    let sql = "UPDATE " + this._table + " SET " + setClauses.join(", ");

    if (this._wheres.length > 0) {
      const whereClauses: string[] = [];
      for (let i = 0; i < this._wheres.length; i++) {
        const w = this._wheres[i];
        params.push(w.value);
        whereClauses.push(w.field + " " + w.op + " $" + params.length);
      }
      sql += " WHERE " + whereClauses.join(" AND ");
    }

    const c = await getConnection();
    await c.query(sql, params);
  }

  async delete(): Promise<void> {
    const params: any[] = [];
    let sql = "DELETE FROM " + this._table;

    if (this._wheres.length > 0) {
      const clauses: string[] = [];
      for (let i = 0; i < this._wheres.length; i++) {
        const w = this._wheres[i];
        params.push(w.value);
        clauses.push(w.field + " " + w.op + " $" + params.length);
      }
      sql += " WHERE " + clauses.join(" AND ");
    }

    const c = await getConnection();
    await c.query(sql, params);
  }

  /// Raw SQL escape hatch — returns rows.
  async raw(sql: string, params?: any[]): Promise<any[]> {
    const c = await getConnection();
    const result = params !== undefined && params.length > 0
      ? await c.query(sql, params)
      : await c.query(sql);
    return result.rows;
  }
}

export const db = new QueryBuilder();
