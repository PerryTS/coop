// @perch/runtime — Typed query builder.
//
// Wraps Perry's pg stdlib with a fluent API that generates parameterized
// SQL. The connection URL is read from PERCH_DB_URL (set by perch-worker
// before spawning the deployment process). Schema namespacing is handled
// by perch-worker setting the Postgres role's default search_path to
// `deployment_<name>`, so the runtime library doesn't inject SET
// search_path — all queries are scoped by the connection's default schema.
//
// Usage:
//   import { db } from "@perch/runtime";
//
//   const user = await db.table("users").where({ id: 42 }).first();
//   await db.table("events").insert({ type: "signup", user_id: 42 });
//   await db.table("users").where({ id: 42 }).update({ name: "Ralph" });
//   await db.table("old").where({ expired: true }).delete();
//
// Note: Perry's pg stdlib parameterized queries are a Phase B gate item.
// Until that lands, this module provides the query builder API surface
// that generates the correct SQL — the actual execution is a stub that
// logs the query and returns an empty result.

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
        // Operator form: { age: { gt: 18 } }
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

  // Build the SQL + params for a SELECT query.
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

    if (this._groupBy) {
      sql += " GROUP BY " + this._groupBy;
    }
    if (this._orderBy) {
      sql += " ORDER BY " + this._orderBy + " " + this._orderDir;
    }
    if (this._limit !== null) {
      sql += " LIMIT " + this._limit;
    }
    if (this._offset !== null) {
      sql += " OFFSET " + this._offset;
    }

    return { sql, params };
  }

  // Execute a SELECT and return all rows.
  async all(): Promise<any[]> {
    const { sql, params } = this.buildSelect();
    return executeQuery(sql, params);
  }

  // Execute a SELECT and return the first row, or null.
  async first(): Promise<any | null> {
    this._limit = 1;
    const rows = await this.all();
    return rows.length > 0 ? rows[0] : null;
  }

  // INSERT a row.
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
    await executeQuery(sql, params);
  }

  // UPDATE rows matching the where clause.
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

    await executeQuery(sql, params);
  }

  // DELETE rows matching the where clause.
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

    await executeQuery(sql, params);
  }

  // Raw SQL escape hatch.
  async raw(sql: string, params: any[] = []): Promise<any[]> {
    return executeQuery(sql, params);
  }
}

// Stub execution — logs the query and returns empty results until
// Perry's pg parameterized query support (Phase B) is wired.
async function executeQuery(sql: string, params: any[]): Promise<any[]> {
  // TODO: Replace with real Perry pg stdlib call:
  //   import { Pool } from "pg";
  //   const pool = new Pool({ connectionString: process.env.PERCH_DB_URL });
  //   const result = await pool.query(sql, params);
  //   return result.rows;
  console.log(JSON.stringify({ _perch_db: true, sql: sql, params: params }));
  return [];
}

export const db = new QueryBuilder();
