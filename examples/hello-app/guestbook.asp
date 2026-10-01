<!DOCTYPE html>
<html><head><title>Guestbook</title></head><body>
<h1>Guestbook</h1>
<%
Set conn = Server.CreateObject("ADODB.Connection")
conn.Open "data/guestbook.db"
conn.Execute "CREATE TABLE IF NOT EXISTS entries (id INTEGER PRIMARY KEY, name TEXT)"
name = Request.QueryString("name")
If name <> "" Then
    Set cmd = Server.CreateObject("ADODB.Command")
    cmd.ActiveConnection = conn
    cmd.CommandText = "INSERT INTO entries (name) VALUES (?)"
    Set param = cmd.CreateParameter("name", 200, 1)
    param.Value = name
    Set ps = cmd.Parameters
    ps.Append param
    cmd.Execute
End If
Set rs = conn.Execute("SELECT name FROM entries ORDER BY id")
Do While Not rs.EOF
    Response.Write "<li>" & rs.Fields("name").Value & "</li>"
    rs.MoveNext
Loop
conn.Close
%>
</body></html>
