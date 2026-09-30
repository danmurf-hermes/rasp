<%@ Language=VBScript %>
<%
' A dictionary keeps key/value pairs for the life of the page.
Set d = Server.CreateObject("Scripting.Dictionary")
d.Add "tea", "1.20"
d.Add "coffee", "1.40"
d.Add "cake", "2.10"
keys = d.Keys
prices = d.Items
%>
<ul>
<% For i = 0 To d.Count - 1 %><li><%= keys(i) %>: <%= prices(i) %></li><% Next %>
</ul>
<p><%= d.Count %> items</p>