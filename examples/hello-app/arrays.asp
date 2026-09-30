<%@ Language=VBScript %>
<%
Dim totals(3)
names = Array("ann", "bob", "cid")
For i = 0 To 2
  totals(i) = (i + 1) * 10
Next
%>
<ul>
<% For i = 0 To 2 %><li><%= names(i) %>: <%= totals(i) %></li><% Next %>
</ul>
<% Response.Write "" %>