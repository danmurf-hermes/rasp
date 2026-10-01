<%@ Language=VBScript %>
<%
' Visitor picks a nickname; the session remembers it.
If Request.QueryString("as") <> "" Then
  Session("who") = Request.QueryString("as")
End If
%>
<p>hello, <%= Session("who") %></p>