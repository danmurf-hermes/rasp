<%@ Language=VBScript %>
<%
' List every file this folder holds, using the sandboxed
' FileSystemObject (paths stay inside the application root).
Set fso = Server.CreateObject("Scripting.FileSystemObject")
names = fso.ListFolder(".")
%>
<ul>
<% For i = 0 To UBound(names) %><li><%= names(i) %></li><% Next %>
</ul>